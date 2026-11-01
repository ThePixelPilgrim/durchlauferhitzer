//! The descriptor budget (NFR-RES-05) at startup, in the built binary
//! with `RLIMIT_NOFILE` lowered in the child (`setrlimit` before `exec`):
//! one descriptor short of the budget is exit 2 naming the needed and the
//! available numbers (Review Focus 5); exactly the budget starts; a soft
//! limit below it is raised when the hard limit allows. Run as non-root
//! (as `nobody` when the tests run as root), so the hard limit binds.
//! After startup, the soft limit lowered with `prlimit --pid` (Review
//! Focus 1, rare paths under seccomp kill): `EMFILE` on accept pauses
//! the listener and the 1 s retry serves the waiting client; on a file
//! open and an upstream socket it is logged.
//!
//! The budget (O5: the descriptors open at the check, plus what the run
//! adds), with `max_connections` = 16 and one listener:
//! 4 open (fds 0–2, the listener) + 3 (epoll, wake eventfd, signalfd)
//! + 2 × 16 (client sockets) = 39, then
//! - a proxy route: + 16 (upstream sockets) = 55;
//! - a static route: + 1 (the root, open) + 1 (the pool's wake
//!   duplicate) + 16 (files) + 4 (`disk_threads`: closed files whose
//!   readahead a worker still runs, OA1) = 61;
//! - both: 77;
//!
//! plus every descriptor this test process passes on to its children
//! (`child::inheritable_fds`: two on GitHub's runners).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use child::{
    Containment, Lines, PATIENCE, PROXY_ROUTE, Running, Setup, TempDir, assert_exit, free_port,
    run, text,
};

const LIMITS: &str = "limits { max_connections 16; }";

/// The three configs, each with its budget.
fn configs(dir: &TempDir, addr: SocketAddr) -> Vec<(String, u64)> {
    let www = dir.dir("www");
    let root = www.display();
    let proxy = format!("listen {addr};\n{LIMITS}\nhost a {{\n  path / {{ {PROXY_ROUTE} }}\n}}\n");
    let stat = format!("listen {addr};\n{LIMITS}\nhost a {{\n  path / {{ static {root}; }}\n}}\n");
    let both = format!(
        "listen {addr};\n{LIMITS}\nhost a {{\n  path / {{ static {root}; }}\n  \
         path /api {{ {PROXY_ROUTE} }}\n}}\n"
    );
    // Whatever this process passes on is open in the server too.
    let inherited = child::inheritable_fds(&[]);
    [(proxy, 55), (stat, 61), (both, 77)]
        .into_iter()
        .enumerate()
        .map(|(i, (conf, needed))| {
            let path = dir.config(&format!("budget{i}.conf"), &conf, Containment::Switch);
            (path.to_str().unwrap().to_owned(), needed + inherited)
        })
        .collect()
}

fn limited(soft: u64, hard: u64) -> Setup {
    Setup {
        nofile: Some((soft, hard)),
        ..Setup::non_root()
    }
}

#[test]
fn one_descriptor_short_is_exit_2_with_both_numbers() {
    let dir = TempDir::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    for (path, needed) in configs(&dir, addr) {
        let short = needed - 1;
        let output = run(&[&path], limited(short, short));
        assert_exit(&output, 2);
        assert_eq!(
            text(&output.stderr),
            format!(
                "durchlauferhitzer: too few file descriptors (RLIMIT_NOFILE): \
                 the server needs {needed}, {short} are available\n"
            ),
            "{path}"
        );
    }
}

#[test]
fn exactly_the_budget_starts() {
    let dir = TempDir::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    for (path, needed) in configs(&dir, addr) {
        let server = Running::start(&[&path], limited(needed, needed), addr);
        // The listener accepts before the check: an answer (404, the host
        // is not `example.test`) shows startup got past it.
        let response = child::get(addr, "/");
        assert!(response.starts_with("HTTP/1.1 404 "), "{path}: {response}");
        drop(server);
    }
}

/// The soft limit is raised to the budget when the hard limit allows it.
#[test]
fn a_low_soft_limit_is_raised() {
    let dir = TempDir::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let (path, needed) = configs(&dir, addr).swap_remove(0);
    let server = Running::start(&[&path], limited(20, 200), addr);
    // The listener accepts before the check (it comes after binding): an
    // answered request (any answer: the host is not `example.test`)
    // shows startup is over.
    let response = child::get(addr, "/");
    assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
    let limits = fs::read_to_string(format!("/proc/{}/limits", server.pid())).unwrap();
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .unwrap();
    let numbers: Vec<&str> = line.split_whitespace().skip(3).take(2).collect();
    assert_eq!(numbers, [needed.to_string(), "200".to_owned()], "{line}");
}

/// O5 (owner decision 2026-10-28): the budget counts the descriptors
/// really open at the check, so a stray inherited one raises it by one.
#[test]
fn an_inherited_descriptor_raises_the_need_by_one() {
    let dir = TempDir::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let (path, needed) = configs(&dir, addr).swap_remove(0);
    // The stray descriptor replaces an inherited fd 3, if there is one.
    let needed = needed - (child::inheritable_fds(&[]) - child::inheritable_fds(&[3]));
    let stray = || Some(std::os::fd::OwnedFd::from(fs::File::open(&path).unwrap()));
    let setup = |limit| Setup {
        stray_fd: stray(),
        ..limited(limit, limit)
    };
    let output = run(&[&path], setup(needed));
    assert_exit(&output, 2);
    assert_eq!(
        text(&output.stderr),
        format!(
            "durchlauferhitzer: too few file descriptors (RLIMIT_NOFILE): \
             the server needs {}, {needed} are available\n",
            needed + 1
        )
    );
    let server = Running::start(&[&path], setup(needed + 1), addr);
    let response = child::get(addr, "/");
    assert!(response.starts_with("HTTP/1.1 404 "), "{response}");
    drop(server);
}

/// `tool` (`prlimit`, or `setpriv` as root) is missing: skips the test
/// with a printed reason, or fails it with
/// `DURCHLAUFERHITZER_REQUIRE_ROOT=1` (CI's root step, which also runs
/// under the seccomp kill switch).
fn skip_without(tool: &str) {
    const REQUIRE: &str = "DURCHLAUFERHITZER_REQUIRE_ROOT";
    assert!(
        std::env::var_os(REQUIRE).is_none_or(|v| v != "1"),
        "no `{tool}` here, but {REQUIRE}=1 requires the test"
    );
    eprintln!("skipped: no `{tool}` here");
}

/// Sets the server's soft `RLIMIT_NOFILE` with `prlimit --pid`, run as
/// the server's user: as `nobody` (`setpriv`) when the tests run as
/// root, which may lack `CAP_SYS_RESOURCE`. `false` when a tool is
/// missing (the caller skips).
fn set_soft_nofile(pid: u32, soft: u64) -> bool {
    let mut cmd = if child::is_root() {
        let id = child::NOBODY.to_string();
        let mut cmd = Command::new("setpriv");
        cmd.args([
            &format!("--reuid={id}"),
            &format!("--regid={id}"),
            "--clear-groups",
            "prlimit",
        ]);
        cmd
    } else {
        Command::new("prlimit")
    };
    cmd.args(["--pid", &pid.to_string(), &format!("--nofile={soft}:")]);
    let tool = if child::is_root() {
        "setpriv"
    } else {
        "prlimit"
    };
    match cmd.output() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            skip_without(tool);
            false
        }
        Err(err) => panic!("{tool}: {err}"),
        // 127: `setpriv` ran but could not exec `prlimit` (a plain
        // `prlimit` that is missing is `NotFound` above).
        Ok(out) if out.status.code() == Some(127) => {
            skip_without("prlimit");
            false
        }
        Ok(out) => {
            assert!(out.status.success(), "{tool}: {}", text(&out.stderr));
            true
        }
    }
}

/// The server's soft `RLIMIT_NOFILE`.
fn soft_nofile(pid: u32) -> u64 {
    let limits = fs::read_to_string(format!("/proc/{pid}/limits")).unwrap();
    let line = limits
        .lines()
        .find(|line| line.starts_with("Max open files"))
        .unwrap();
    line.split_whitespace().nth(3).unwrap().parse().unwrap()
}

/// The lowest descriptor number `pid` has free, once its descriptors
/// have not changed for 200 ms (the readiness check's connection is
/// closed by then). With the soft limit there, no descriptor can be
/// opened; one above it, exactly one.
fn lowest_free_fd(pid: u32) -> u64 {
    let open = || -> BTreeSet<u64> {
        fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
            .collect()
    };
    let deadline = Instant::now() + PATIENCE;
    let mut last = open();
    loop {
        thread::sleep(Duration::from_millis(200));
        let now = open();
        if now == last {
            break;
        }
        assert!(Instant::now() < deadline, "descriptors never settled");
        last = now;
    }
    (0..).find(|fd| !last.contains(fd)).unwrap()
}

/// A request for `path` on `stream` (host `a`, as the configs have it).
fn request(stream: &mut TcpStream, path: &str) {
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: a\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .unwrap();
}

fn response(stream: &mut TcpStream) -> String {
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    text(&out)
}

/// A server for config `which` of [`configs`] (0 proxy, 1 static) with
/// `small.txt` in its root, started without a lowered limit.
fn started(which: usize) -> (TempDir, SocketAddr, Running, Lines) {
    let dir = TempDir::new();
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let (path, _) = configs(&dir, addr).swap_remove(which);
    dir.file("www/small.txt", "small\n");
    let mut server = Running::start(&[&path], Setup::non_root(), addr);
    let lines = server.lines();
    (dir, addr, server, lines)
}

/// DRV-16 in the binary, under seccomp kill in CI (Review Focus 1):
/// out of descriptors after startup, `accept` fails with `EMFILE`
/// (logged), the listener pauses, and once descriptors are available
/// again the 1 s retry accepts the waiting connection and serves it.
#[test]
fn emfile_on_accept_pauses_and_the_retry_serves() {
    let (_dir, addr, server, mut lines) = started(1);
    let pid = server.pid();
    let soft = soft_nofile(pid);
    if !set_soft_nofile(pid, lowest_free_fd(pid)) {
        return;
    }
    let mut client = TcpStream::connect(addr).unwrap();
    request(&mut client, "/small.txt");
    lines.wait_for("kind=accept_failed errno=24");
    assert!(set_soft_nofile(pid, soft));
    let raised = Instant::now();
    let answer = response(&mut client);
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.ends_with("\r\n\r\nsmall\n"), "{answer}");
    let elapsed = raised.elapsed();
    assert!(elapsed < Duration::from_millis(1500), "{elapsed:?}");
}

/// One descriptor left after startup: the client takes it, and opening
/// the file fails with `EMFILE` (logged); the client gets a 404.
#[test]
fn emfile_on_open_is_logged() {
    let (_dir, addr, server, mut lines) = started(1);
    let pid = server.pid();
    if !set_soft_nofile(pid, lowest_free_fd(pid) + 1) {
        return;
    }
    let mut client = TcpStream::connect(addr).unwrap();
    request(&mut client, "/small.txt");
    lines.wait_for("kind=open_failed errno=24");
    let answer = response(&mut client);
    assert!(answer.starts_with("HTTP/1.1 404 Not Found\r\n"), "{answer}");
}

/// One descriptor left after startup: the client takes it, and the
/// upstream socket fails with `EMFILE` (logged); the client gets a 502.
#[test]
fn emfile_on_the_upstream_socket_is_logged() {
    let (_dir, addr, server, mut lines) = started(0);
    let pid = server.pid();
    if !set_soft_nofile(pid, lowest_free_fd(pid) + 1) {
        return;
    }
    let mut client = TcpStream::connect(addr).unwrap();
    request(&mut client, "/");
    lines.wait_for("kind=socket_failed errno=24");
    let answer = response(&mut client);
    assert!(
        answer.starts_with("HTTP/1.1 502 Bad Gateway\r\n"),
        "{answer}"
    );
}
