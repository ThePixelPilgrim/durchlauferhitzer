//! The privilege rules (FR-PROC-03, A8) of the built binary:
//!
//! - as non-root (as `nobody` when the tests run as root) with `user`
//!   set and every port at or above 1024, nothing is dropped, the server
//!   runs and logs `kind=privileges action=kept reason=not_root`;
//! - as non-root, a refused bind to a port below 1024 is exit 2 naming it;
//! - as root (these tests skip with a printed reason unless the tests run
//!   as root, and fail instead with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`,
//!   which CI's root step sets): `user` unset is exit 2, an unknown
//!   user is exit 2, and with `user nobody` the server drops to nobody
//!   (`drop_to`, in this subprocess only) and serves; root is any uid 0
//!   (O6), so a real uid of 0 under an effective nobody counts as root.

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

/// The environment variable that turns a root skip into a failure (CI's
/// root step sets it, so that step can never pass by skipping).
const REQUIRE_ROOT: &str = "DURCHLAUFERHITZER_REQUIRE_ROOT";

/// Whether a root-only test runs; prints why it skips otherwise, or fails
/// when [`REQUIRE_ROOT`] is `1`.
fn root_or_skip() -> bool {
    if is_root() {
        return true;
    }
    if std::env::var_os(REQUIRE_ROOT).is_some_and(|v| v == "1") {
        panic!(
            "not root (euid {}), but {REQUIRE_ROOT}=1 requires it",
            durchlauferhitzer::sys::user::euid()
        );
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

/// Owner decision 2026-10-28 (O3, FR-PROC-03): root with a socket from
/// systemd and no `user` is exit 2 too; the socket changes nothing.
#[test]
fn as_root_with_a_systemd_socket_and_no_user_is_exit_2() {
    if !root_or_skip() {
        return;
    }
    let listener = std::net::TcpListener::bind(loopback(0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = TempDir::new();
    let path = static_config(&dir, addr, "");
    let setup = Setup {
        listen_fd: Some(std::os::fd::OwnedFd::from(listener)),
        ..Setup::default()
    };
    let output = run(&[&path], setup);
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: running as root without `user`: \
         the server never keeps root after binding\n"
    );
}

/// Root is any uid 0 (owner decision 2026-10-28, O6): a process whose
/// real uid is 0 but whose effective (and so saved) uid is nobody's
/// counts as root, so `user` is required.
#[test]
fn a_real_uid_of_0_is_root_without_a_user_exit_2() {
    if !root_or_skip() {
        return;
    }
    let dir = TempDir::new();
    let path = static_config(&dir, loopback(free_port()), "");
    let setup = Setup {
        effective_uid: Some(NOBODY),
        ..Setup::default()
    };
    let output = run(&[&path], setup);
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: running as root without `user`: \
         the server never keeps root after binding\n"
    );
}

/// O6: with a real uid of 0 and `user nobody`, every uid ends up
/// nobody's (the drop takes the effective uid back to 0 first).
#[test]
fn a_real_uid_of_0_drops_every_uid() {
    if !root_or_skip() {
        return;
    }
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, addr, "user nobody;");
    let setup = Setup {
        effective_uid: Some(NOBODY),
        ..Setup::default()
    };
    let server = Running::start(&[&path], setup, addr);
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let status = fs::read_to_string(format!("/proc/{}/status", server.pid())).unwrap();
    let uids = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .unwrap();
    let uids: Vec<u32> = uids
        .split_whitespace()
        .skip(1)
        .map(|id| id.parse().unwrap())
        .collect();
    assert_eq!(uids, [NOBODY; 4], "{status}");
    let stderr = server.kill();
    assert!(!stderr.contains("kind=privileges"), "{stderr}");
}
