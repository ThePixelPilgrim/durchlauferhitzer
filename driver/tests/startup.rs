//! The startup sequence (FR-PROC-04, A11) of the built binary, each test
//! in a subprocess run as non-root (as `nobody` when the tests run as
//! root): it serves, a missing static root or a port in use is exit 2
//! naming it (Review Focus 5), the signals are blocked in every thread
//! (A9), and no socket lands on fds 0–2.

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
use std::net::{SocketAddr, TcpListener};

use child::{
    PROXY_ROUTE, Running, Setup, TempDir, assert_exit, config, fd_target, free_port, get, run, text,
};

/// A config serving `dir/www` on `listen`, written to `dir`.
fn static_config(dir: &TempDir, listen: &str) -> String {
    let www = dir.dir("www");
    fs::write(www.join("index.html"), "hello\n").unwrap();
    let conf = config(listen, "", &format!("static {};", www.display()));
    let path = dir.file("static.conf", &conf);
    path.to_str().unwrap().to_owned()
}

fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[test]
fn the_server_serves_a_static_file() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let server = Running::start(&[&path], Setup::non_root(), addr);
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(response.ends_with("\r\n\r\nhello\n"), "{response}");
    let stderr = server.kill();
    // Conn 1 was the readiness probe's empty connection.
    assert!(
        stderr.contains(
            "kind=response conn=2 peer=127.0.0.1 status=200 method=GET host=example.test \
             path=/index.html\n"
        ),
        "{stderr}"
    );
}

#[test]
fn a_missing_static_root_is_exit_2_naming_it() {
    let dir = TempDir::new();
    let missing = dir.path().join("no-such-root");
    let conf = config(
        &loopback(free_port()).to_string(),
        "",
        &format!("static {};", missing.display()),
    );
    let path = dir.file("missing.conf", &conf);
    let output = run(&[path.to_str().unwrap()], Setup::non_root());
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "durchlauferhitzer: static root {}: ",
            missing.display()
        )),
        "{stderr}"
    );
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
}

#[test]
fn a_root_that_is_a_file_is_exit_2_naming_it() {
    let dir = TempDir::new();
    let file = dir.file("plain", "not a directory");
    let conf = config(
        &loopback(free_port()).to_string(),
        "",
        &format!("static {};", file.display()),
    );
    let path = dir.file("file.conf", &conf);
    let output = run(&[path.to_str().unwrap()], Setup::non_root());
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "durchlauferhitzer: static root {}: ",
            file.display()
        )),
        "{stderr}"
    );
}

#[test]
fn a_port_in_use_is_exit_2_naming_the_address() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    let dir = TempDir::new();
    let path = dir.file("taken.conf", &config(&addr.to_string(), "", PROXY_ROUTE));
    let output = run(&[path.to_str().unwrap()], Setup::non_root());
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!("durchlauferhitzer: cannot listen on {addr}: ")),
        "{stderr}"
    );
    assert!(stderr.contains("in use"), "{stderr}");
}

/// A6: binding goes in config order and the first failure names its
/// address: `0.0.0.0:P` then `127.0.0.1:P` fails at the second.
#[test]
fn overlapping_listen_addresses_fail_at_the_second() {
    let port = free_port();
    let dir = TempDir::new();
    let path = dir.file(
        "overlap.conf",
        &config(&format!("0.0.0.0:{port} 127.0.0.1:{port}"), "", PROXY_ROUTE),
    );
    let output = run(&[path.to_str().unwrap()], Setup::non_root());
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!(
            "durchlauferhitzer: cannot listen on 127.0.0.1:{port}: "
        )),
        "{stderr}"
    );
}

/// The signal mask (A9) is set before the disk workers start, so every
/// thread blocks SIGTERM, SIGINT and SIGPIPE: SIGPIPE can never kill the
/// process (signal handling itself is Task 9's).
#[test]
fn every_thread_blocks_the_handled_signals() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let server = Running::start(&[&path], Setup::non_root(), addr);
    let wanted: u64 = [libc::SIGTERM, libc::SIGINT, libc::SIGPIPE]
        .iter()
        .map(|&signo| 1u64 << (signo - 1))
        .sum();
    let tasks: Vec<_> = fs::read_dir(format!("/proc/{}/task", server.pid()))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    // The main thread and the 4 disk workers (`disk_threads` default).
    assert_eq!(tasks.len(), 5, "{tasks:?}");
    for task in tasks {
        let status = fs::read_to_string(task.join("status")).unwrap();
        let blocked = status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:"))
            .unwrap();
        let blocked = u64::from_str_radix(blocked.trim(), 16).unwrap();
        assert_eq!(blocked & wanted, wanted, "{}: {blocked:x}", task.display());
    }
}

/// `disk_threads` is passed on: 2 workers, 3 threads in all.
#[test]
fn disk_threads_sets_the_worker_count() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let www = dir.dir("www");
    let conf = config(
        &addr.to_string(),
        "limits { disk_threads 2; }",
        &format!("static {};", www.display()),
    );
    let path = dir.file("threads.conf", &conf);
    let server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    let tasks = fs::read_dir(format!("/proc/{}/task", server.pid()))
        .unwrap()
        .count();
    assert_eq!(tasks, 3);
}

/// Closed fds 0, 1 and 2 are open on `/dev/null` by the time `main` runs
/// (the Rust runtime reopens them), so the listener cannot land on one.
/// This pins the runtime's behaviour the startup relies on (FR-PROC-04).
#[test]
fn no_socket_lands_on_a_standard_descriptor() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let setup = Setup {
        close: vec![0, 1, 2],
        ..Setup::non_root()
    };
    let server = Running::start(&[&path], setup, addr);
    for fd in 0..3 {
        assert_eq!(
            fd_target(server.pid(), fd).as_deref(),
            Some("/dev/null"),
            "fd {fd}"
        );
    }
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
}
