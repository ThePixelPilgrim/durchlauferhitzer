//! The descriptor budget (NFR-RES-05) at startup, in the built binary
//! with `RLIMIT_NOFILE` lowered in the child (`setrlimit` before `exec`):
//! one descriptor short of the budget is exit 2 naming the needed and the
//! available numbers (Review Focus 5); exactly the budget starts; a soft
//! limit below it is raised when the hard limit allows. Run as non-root
//! (as `nobody` when the tests run as root), so the hard limit binds.
//!
//! The budget (O5: the descriptors open at the check, plus what the run
//! adds), with `max_connections` = 16 and one listener:
//! 4 open (fds 0–2, the listener) + 3 (epoll, wake eventfd, signalfd)
//! + 2 × 16 (client sockets) = 39, then
//! - a proxy route: + 16 (upstream sockets) = 55;
//! - a static route: + 1 (the root, open) + 1 (the pool's wake
//!   duplicate) + 16 (files) = 57;
//! - both: 73;
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

use std::fs;
use std::net::SocketAddr;

use child::{PROXY_ROUTE, Running, Setup, TempDir, assert_exit, free_port, run, text};

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
    [(proxy, 55), (stat, 57), (both, 73)]
        .into_iter()
        .enumerate()
        .map(|(i, (conf, needed))| {
            let path = dir.file(&format!("budget{i}.conf"), &conf);
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
