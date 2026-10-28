//! The privilege rules (FR-PROC-03, A8) of the built binary:
//!
//! - as non-root (as `nobody` when the tests run as root) with `user`
//!   set and every port at or above 1024, nothing is dropped, the server
//!   runs and logs `kind=privileges action=kept reason=not_root`;
//! - as non-root, a refused bind to a port below 1024 is exit 2 naming it;
//! - as root (these tests skip with a printed reason unless the tests run
//!   as root; GitHub's runners do not): `user` unset is exit 2, an unknown
//!   user is exit 2, and with `user nobody` the server drops to nobody
//!   (`drop_to`, in this subprocess only) and serves.

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
use std::net::SocketAddr;

use child::{
    NOBODY, PROXY_ROUTE, Running, Setup, TempDir, assert_exit, config, free_port, get, is_root,
    run, text,
};

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// Whether a root-only test runs; prints why it skips otherwise.
fn root_or_skip() -> bool {
    if is_root() {
        return true;
    }
    eprintln!(
        "skipped: needs root (euid {}); the root-only privilege tests run only as root",
        durchlauferhitzer::sys::user::euid()
    );
    false
}

/// A config serving `dir/www` (one file) on `addr` with `extra` lines.
fn static_config(dir: &TempDir, addr: SocketAddr, extra: &str) -> String {
    let www = dir.dir("www");
    fs::write(www.join("index.html"), "hello\n").unwrap();
    let path = dir.file(
        "privileges.conf",
        &config(
            &addr.to_string(),
            extra,
            &format!("static {};", www.display()),
        ),
    );
    path.to_str().unwrap().to_owned()
}

#[test]
fn as_non_root_with_a_user_nothing_is_dropped_and_that_is_logged() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, addr, "user nobody;");
    let server = Running::start(&[&path], Setup::non_root(), addr);
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let stderr = server.kill();
    assert!(
        stderr.starts_with("kind=privileges action=kept reason=not_root\n"),
        "{stderr}"
    );
}

/// A8 as amended (owner decision 2026-10-28, O7): no up-front check of
/// the port numbers; a bind refused with `EACCES` is the startup error
/// naming its address (A6). Skipped where the host lets non-root bind
/// port 1023 (`net.ipv4.ip_unprivileged_port_start` at or below it).
#[test]
fn as_non_root_a_privileged_port_is_exit_2_naming_it() {
    let start: u16 = fs::read_to_string("/proc/sys/net/ipv4/ip_unprivileged_port_start")
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    if start <= 1023 {
        eprintln!("skipped: ip_unprivileged_port_start is {start}, so non-root may bind 1023");
        return;
    }
    let dir = TempDir::new();
    let path = dir.file(
        "low.conf",
        &config(
            &format!("127.0.0.1:{} 127.0.0.1:1023", free_port()),
            "",
            PROXY_ROUTE,
        ),
    );
    let output = run(&[path.to_str().unwrap()], Setup::non_root());
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: cannot listen on 127.0.0.1:1023: Permission denied (os error 13)\n"
    );
}

#[test]
fn as_root_without_a_user_is_exit_2() {
    if !root_or_skip() {
        return;
    }
    let dir = TempDir::new();
    let path = static_config(&dir, loopback(free_port()), "");
    let output = run(&[&path], Setup::default());
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: running as root without `user`: \
         the server never keeps root after binding\n"
    );
}

#[test]
fn as_root_an_unknown_user_is_exit_2() {
    if !root_or_skip() {
        return;
    }
    let dir = TempDir::new();
    let path = static_config(&dir, loopback(free_port()), "user no_such_user_dlh;");
    let output = run(&[&path], Setup::default());
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: user no_such_user_dlh: no such user\n"
    );
}

/// `drop_to` for real, in the server's process: every uid and gid is
/// nobody's, the supplementary groups are only its group, and the server
/// still serves its root (opened before the drop).
#[test]
fn as_root_the_server_drops_to_the_user() {
    if !root_or_skip() {
        return;
    }
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, addr, "user nobody;");
    let server = Running::start(&[&path], Setup::default(), addr);
    // The listener accepts before the drop (it comes after binding): a
    // served request shows startup is over.
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let status = fs::read_to_string(format!("/proc/{}/status", server.pid())).unwrap();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .unwrap()
            .split_whitespace()
            .map(|id| id.parse::<u32>().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(field("Uid:"), [NOBODY; 4], "{status}");
    assert_eq!(field("Gid:"), [NOBODY; 4], "{status}");
    assert_eq!(field("Groups:"), [NOBODY], "{status}");
    let stderr = server.kill();
    assert!(!stderr.contains("kind=privileges"), "{stderr}");
}

/// FR-PROC-02: with a socket from systemd a port below 1024 needs no root.
/// Root binds it here and passes it to a server running as `nobody`.
#[test]
fn a_systemd_socket_below_1024_needs_no_root() {
    if !root_or_skip() {
        return;
    }
    let listener = (900..1024)
        .find_map(|port| std::net::TcpListener::bind(loopback(port)).ok())
        .expect("no free port below 1024");
    let addr = listener.local_addr().unwrap();
    let dir = TempDir::new();
    let path = static_config(&dir, addr, "user nobody;");
    let setup = Setup {
        listen_fd: Some(std::os::fd::OwnedFd::from(listener)),
        as_nobody: true,
        ..Setup::default()
    };
    let server = Running::start(&[&path], setup, addr);
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let stderr = server.kill();
    assert!(
        stderr.starts_with("kind=privileges action=kept reason=not_root\n"),
        "{stderr}"
    );
}
