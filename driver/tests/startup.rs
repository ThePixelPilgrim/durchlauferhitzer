//! The startup sequence (FR-PROC-04, A11) of the built binary, each test
//! in a subprocess run as non-root (as `nobody` when the tests run as
//! root): it serves, a missing static root or a port in use is exit 2
//! naming it (Review Focus 5), the signals are blocked in every thread
//! (A9), and no socket lands on fds 0–2. Shared ports and readiness
//! (FR-PROC-07): `--ready-fd` gets `ready\n` then EOF, or EOF alone when
//! startup fails; a second normal start fails with `EADDRINUSE`, a
//! `--ready-fd` start shares the port and, once ready, takes its new
//! connections.

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
use std::io::Read as _;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::OwnedFd;
use std::sync::mpsc;

use child::{
    Containment, Filter, PATIENCE, PROXY_ROUTE, Running, Setup, TempDir, assert_exit, command,
    config, fd_target, free_port, get, memory_backstop_line, non_root_uid, run, text, threads,
};
use durchlauferhitzer::sys::fd::pipe;

/// A config serving `dir/www` on `listen`, written to `dir`.
fn static_config(dir: &TempDir, listen: &str) -> String {
    let www = dir.dir("www");
    fs::write(www.join("index.html"), "hello\n").unwrap();
    let conf = config(listen, "", &format!("static {};", www.display()));
    let path = dir.config("static.conf", &conf, Containment::Switch);
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
    // The memory backstop first (NFR-RES-09); owner decision 2026-11-06:
    // bound as non-root, the ports can be joined by any process of that
    // uid; logged next (no `user`).
    assert!(
        stderr.starts_with(&format!(
            "{}\nkind=shared_ports uid={}\n",
            memory_backstop_line(&path),
            non_root_uid()
        )),
        "{stderr}"
    );
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
    let path = dir.config("missing.conf", &conf, Containment::Switch);
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

/// FR-CFG-26: an `internal_files` root is opened at startup like a static
/// root, so a missing one is exit 2 with the same wording.
#[test]
fn missing_internal_files_root_is_exit_2() {
    let dir = TempDir::new();
    let missing = dir.path().join("no-such-internal-root");
    let conf = config(
        &loopback(free_port()).to_string(),
        "",
        &format!(
            "{PROXY_ROUTE} internal_files x-sendfile {};",
            missing.display()
        ),
    );
    let path = dir.config("internal.conf", &conf, Containment::Switch);
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

/// NFR-PORT-03 (owner decision 2026-10-29, O3): on a kernel without
/// `openat2` (a seccomp filter makes it fail with `ENOSYS`), the probe
/// stops startup: exit 2 with its message.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[test]
fn without_openat2_startup_is_exit_2() {
    let dir = TempDir::new();
    let path = static_config(&dir, &loopback(free_port()).to_string());
    let setup = Setup {
        filter: Filter::Openat2Enosys,
        ..Setup::non_root()
    };
    let output = run(&[&path], setup);
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        "durchlauferhitzer: the kernel has no openat2 (Linux 5.6 or later is needed)\n"
    );
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
    let path = dir.config("file.conf", &conf, Containment::Switch);
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
    let path = dir.config(
        "taken.conf",
        &config(&addr.to_string(), "", PROXY_ROUTE),
        Containment::Switch,
    );
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
/// address: `0.0.0.0:P` then `127.0.0.1:P` fails at the second. The
/// overlap is checked before any bind, so a `--ready-fd` start (no probe,
/// `SO_REUSEPORT` on both) fails the same way, its descriptor unwritten.
#[test]
fn overlapping_listen_addresses_fail_at_the_second() {
    let port = free_port();
    let dir = TempDir::new();
    let path = dir.config(
        "overlap.conf",
        &config(&format!("0.0.0.0:{port} 127.0.0.1:{port}"), "", PROXY_ROUTE),
        Containment::Switch,
    );
    let path = path.to_str().unwrap();
    // The message is the libc's (musl: "Address in use").
    let in_use = std::io::Error::from_raw_os_error(libc::EADDRINUSE);
    let expected = format!("durchlauferhitzer: cannot listen on 127.0.0.1:{port}: {in_use}\n");
    let output = run(&[path], Setup::non_root());
    assert_exit(&output, 2);
    assert_eq!(text(&output.stderr), expected);
    let (read, write) = pipe().unwrap();
    let output = run(&["--ready-fd", "3", path], ready_setup(write));
    assert_exit(&output, 2);
    assert_eq!(text(&output.stderr), expected);
    assert_eq!(read_to_eof(read), b"");
}

/// The signal mask (A9) is set before the disk workers start, so every
/// thread blocks SIGTERM, SIGINT, SIGPIPE and SIGHUP: SIGPIPE and SIGHUP
/// can never kill the process, and the signalfd gets every one (their
/// handling is in `shutdown.rs`).
#[test]
fn every_thread_blocks_the_handled_signals() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let server = Running::start(&[&path], Setup::non_root(), addr);
    let wanted: u64 = [libc::SIGTERM, libc::SIGINT, libc::SIGPIPE, libc::SIGHUP]
        .iter()
        .map(|&signo| 1u64 << (signo - 1))
        .sum();
    // The main thread and the 4 disk workers (`disk_threads` default).
    let tasks = threads(server.pid(), 5);
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
    let path = dir.config("threads.conf", &conf, Containment::Switch);
    let server = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    let tasks = threads(server.pid(), 3);
    // No more than that come later either.
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(threads(server.pid(), 3).len(), 3, "{tasks:?}");
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

/// Everything written to `read` until EOF (every write end closed), read
/// in a thread; at most [`PATIENCE`].
fn read_to_eof(read: OwnedFd) -> Vec<u8> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = fs::File::from(read).read_to_end(&mut bytes);
        let _ = sender.send(result.map(|_| bytes));
    });
    receiver
        .recv_timeout(PATIENCE)
        .expect("no EOF on the ready pipe")
        .unwrap()
}

/// A setup passing `write` as the child's fd 3 (no systemd variable).
fn ready_setup(write: OwnedFd) -> Setup {
    Setup {
        stray_fd: Some(write),
        ..Setup::non_root()
    }
}

/// Starts `--ready-fd 3 <path>` with a pipe's write end as fd 3 and
/// waits for EOF on it; returns the server and what it wrote.
fn start_with_ready_fd(path: &str) -> (Running, Vec<u8>) {
    let (read, write) = pipe().unwrap();
    let (mut cmd, keep) = command(&["--ready-fd", "3", path], ready_setup(write));
    let server = Running {
        child: cmd.spawn().unwrap(),
    };
    // The parent's copies of the write end go: EOF comes from the server.
    drop(keep);
    let written = read_to_eof(read);
    (server, written)
}

/// FR-PROC-07 step 3: `ready\n` then EOF once startup is complete, and
/// the server serves from then on (no connect probe first).
#[test]
fn ready_fd_gets_ready_then_eof() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let (server, written) = start_with_ready_fd(&path);
    assert_eq!(text(&written), "ready\n");
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let stderr = server.kill();
    assert!(
        stderr.contains("kind=response conn=1 peer=127.0.0.1 status=200"),
        "{stderr}"
    );
    assert!(!stderr.contains("ready_failed"), "{stderr}");
}

/// A start that fails closes the descriptor unwritten (Review Focus 2):
/// before binding (a missing static root) and after it (too few
/// descriptors, NFR-RES-05).
#[test]
fn ready_fd_is_closed_unwritten_when_startup_fails() {
    let dir = TempDir::new();
    let missing = dir.path().join("no-such-root");
    let conf = config(
        &loopback(free_port()).to_string(),
        "",
        &format!("static {};", missing.display()),
    );
    let missing_root = dir.config("missing.conf", &conf, Containment::Switch);
    let too_few = static_config(&dir, &loopback(free_port()).to_string());
    let cases = [
        (missing_root.to_str().unwrap(), None, "static root "),
        (too_few.as_str(), Some((64, 64)), "too few file descriptors"),
    ];
    for (path, nofile, message) in cases {
        let (read, write) = pipe().unwrap();
        let setup = Setup {
            nofile,
            ..ready_setup(write)
        };
        let output = run(&["--ready-fd", "3", path], setup);
        assert_exit(&output, 2);
        let stderr = text(&output.stderr);
        assert!(
            stderr.starts_with(&format!("durchlauferhitzer: {message}")),
            "{stderr}"
        );
        assert_eq!(read_to_eof(read), b"", "{path}");
    }
}

/// FR-CFG-10, Review Focus 4: the listeners share their ports
/// (`SO_REUSEPORT`), but a normal start probes first, so a second one
/// still fails with `EADDRINUSE`, naming the address.
#[test]
fn a_second_normal_start_fails_with_eaddrinuse() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let _first = Running::start(&[&path], Setup::non_root(), addr);
    let output = run(&[&path], Setup::non_root());
    assert_exit(&output, 2);
    let stderr = text(&output.stderr);
    assert!(
        stderr.starts_with(&format!("durchlauferhitzer: cannot listen on {addr}: ")),
        "{stderr}"
    );
    assert!(stderr.contains("in use"), "{stderr}");
}

/// FR-PROC-07 step 2 and "Steering": a `--ready-fd` start skips the
/// probe and joins the running server's port; once ready, it steers every
/// new connection to itself, while the old server still runs.
#[test]
fn a_ready_fd_start_shares_the_port_and_takes_its_new_connections() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let mut old = Running::start(&[&path], Setup::non_root(), addr);
    let (new, written) = start_with_ready_fd(&path);
    assert_eq!(text(&written), "ready\n");
    for _ in 0..64 {
        let response = get(addr, "/index.html");
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    }
    assert!(
        old.child.try_wait().unwrap().is_none(),
        "the old server ended"
    );
    let old = old.kill();
    assert!(!old.contains(" status=200 "), "old: {old}");
    let new = new.kill();
    assert_eq!(new.matches(" status=200 ").count(), 64, "new: {new}");
}

/// A failed `ready\n` write (the reader is gone: `EPIPE`, SIGPIPE being
/// blocked) is logged, and the server serves on. It comes after the
/// `containment` lines: readiness is reported once the seccomp filter is
/// installed (FR-PROC-07 step 3), never before.
#[test]
fn a_failed_ready_write_is_logged_and_the_server_serves_on() {
    let dir = TempDir::new();
    let addr = loopback(free_port());
    let path = static_config(&dir, &addr.to_string());
    let (read, write) = pipe().unwrap();
    drop(read);
    let server = Running::start(&["--ready-fd", "3", &path], ready_setup(write), addr);
    let response = get(addr, "/index.html");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    let stderr = server.kill();
    let lines: Vec<&str> = stderr.lines().collect();
    let ready_failed = lines
        .iter()
        .position(|line| *line == "kind=ready_failed errno=32")
        .unwrap_or_else(|| panic!("{stderr}"));
    let last_containment = lines
        .iter()
        .rposition(|line| line.starts_with("kind=containment layer=seccomp "))
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(last_containment < ready_failed, "{stderr}");
}
