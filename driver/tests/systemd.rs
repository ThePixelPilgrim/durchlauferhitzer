//! systemd socket activation (FR-PROC-02, A7) of the built binary: a
//! pre-bound listener is passed as fd 3 with `LISTEN_PID`/`LISTEN_FDS`
//! (set by `sh -c '... exec'`, so `LISTEN_PID` is the server's own pid).
//! The server serves on it; a socket whose address is not configured, a
//! configured address without a socket, a `LISTEN_PID` that is not the
//! server's and a descriptor that is not a listening socket are exit 2.
//! Run as non-root (as `nobody` when the tests run as root): socket
//! activation never needs root.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::fs::File;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::OwnedFd;

use child::{PROXY_ROUTE, Running, Setup, TempDir, assert_exit, config, free_port, get, run, text};

fn listener() -> (OwnedFd, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (OwnedFd::from(listener), addr)
}

fn activated(fd: OwnedFd) -> Setup {
    Setup {
        listen_fd: Some(fd),
        ..Setup::non_root()
    }
}

#[test]
fn the_server_serves_on_the_socket_systemd_passed() {
    let (fd, addr) = listener();
    let dir = TempDir::new();
    // The route answers 404 for a path it does not have: any answer
    // shows the socket is served.
    let www = dir.dir("www");
    let conf = config(&addr.to_string(), "", &format!("static {};", www.display()));
    let path = dir.file("systemd.conf", &conf);
    let server = Running::start(&[path.to_str().unwrap()], activated(fd), addr);
    let response = get(addr, "/missing");
    assert!(
        response.starts_with("HTTP/1.1 404 Not Found\r\n"),
        "{response}"
    );
    drop(server);
}

#[test]
fn a_socket_for_an_address_not_configured_is_exit_2() {
    let (fd, addr) = listener();
    let other = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let dir = TempDir::new();
    let path = dir.file("other.conf", &config(&other.to_string(), "", PROXY_ROUTE));
    let output = run(&[path.to_str().unwrap()], activated(fd));
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert_eq!(
        stderr,
        format!(
            "durchlauferhitzer: systemd socket fd 3 is bound to {addr}, \
             which is not a listen address\n"
        )
    );
}

#[test]
fn a_configured_address_without_a_socket_is_exit_2() {
    let (fd, addr) = listener();
    let missing = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let dir = TempDir::new();
    let path = dir.file(
        "two.conf",
        &config(&format!("{addr} {missing}"), "", PROXY_ROUTE),
    );
    let output = run(&[path.to_str().unwrap()], activated(fd));
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        format!("durchlauferhitzer: listen {missing} has no socket from systemd\n")
    );
}

#[test]
fn a_listen_pid_that_is_not_ours_is_exit_2() {
    let (fd, addr) = listener();
    let dir = TempDir::new();
    let path = dir.file("pid.conf", &config(&addr.to_string(), "", PROXY_ROUTE));
    let setup = Setup {
        listen_pid: Some("1".to_owned()),
        ..activated(fd)
    };
    let output = run(&[path.to_str().unwrap()], setup);
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with("durchlauferhitzer: LISTEN_PID is 1, not this process ("),
        "{stderr}"
    );
}

#[test]
fn a_descriptor_that_is_not_a_listening_socket_is_exit_2() {
    let dir = TempDir::new();
    let path = dir.file(
        "file.conf",
        &config(&format!("127.0.0.1:{}", free_port()), "", PROXY_ROUTE),
    );
    let file = OwnedFd::from(File::open(&path).unwrap());
    let output = run(&[path.to_str().unwrap()], activated(file));
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: systemd socket fd 3 is not a listening TCP socket\n"
    );
}

/// A connected (not listening) TCP socket is refused too.
#[test]
fn a_connected_socket_is_not_a_listener() {
    let (fd, addr) = listener();
    let stream = std::net::TcpStream::connect(addr).unwrap();
    let dir = TempDir::new();
    let path = dir.file("conn.conf", &config(&addr.to_string(), "", PROXY_ROUTE));
    let output = run(&[path.to_str().unwrap()], activated(OwnedFd::from(stream)));
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: systemd socket fd 3 is not a listening TCP socket\n"
    );
    drop(fd);
}
