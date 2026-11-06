//! `child::free_port` against the race it exists to avoid: a port picked
//! for a server must not be one the kernel can hand to any other socket
//! between the pick and the server's bind (CI, 2026-11-01: a server
//! test lost its port to another socket). The kernel hands out ports on
//! its own (`bind` to port 0, `connect`) only from the ephemeral range,
//! `net.ipv4.ip_local_port_range`. A sample of the ports it hands out
//! lies in that range; `free_port`'s lie outside it. Nor may a child
//! forked by another test thread hold a copy of a probe socket on it:
//! the pick opens none (the fork race below).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test binary; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

use std::fs;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// Ports the kernel hands out in the sample: listeners bound to 0 and
/// the client end of a connection to each, all held at once.
const SAMPLE: usize = 128;

/// `net.ipv4.ip_local_port_range`: the ephemeral ports, inclusive.
fn ephemeral() -> (u16, u16) {
    let range = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    let mut ends = range.split_whitespace().map(|end| end.parse().unwrap());
    (ends.next().unwrap(), ends.next().unwrap())
}

#[test]
fn no_other_socket_can_get_a_free_port() {
    let (low, high) = ephemeral();
    let ephemeral = low..=high;
    let listeners: Vec<TcpListener> = (0..SAMPLE)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let clients: Vec<TcpStream> = listeners
        .iter()
        .map(|l| TcpStream::connect(l.local_addr().unwrap()).unwrap())
        .collect();
    let handed_out = listeners
        .iter()
        .map(|l| l.local_addr().unwrap().port())
        .chain(clients.iter().map(|c| c.local_addr().unwrap().port()));
    for port in handed_out {
        assert!(
            ephemeral.contains(&port),
            "the kernel handed out {port}, outside {low}-{high}"
        );
    }
    for _ in 0..3 {
        let port = child::free_port();
        assert!(
            !ephemeral.contains(&port),
            "{port} is in the ephemeral range {low}-{high}"
        );
        // And it is still free for the server.
        TcpListener::bind(("127.0.0.1", port)).unwrap();
    }
}

/// How long the fork race below runs.
const RACE: Duration = Duration::from_secs(3);

/// CI (arm64, 2026-11-06): a server failed its own bind with
/// `EADDRINUSE` on a port `free_port` had just picked. A child forked by
/// another test thread holds a copy of every descriptor of the test
/// process until its `exec` (`O_CLOEXEC` only closes them there), and the
/// tests' `pre_exec` hooks stretch that window. So the pick must open no
/// socket. Here one thread forks children whose hook closes 100 000
/// descriptors (a long window), while another picks ports and binds each
/// at once, as the server would: every bind succeeds.
#[test]
fn a_child_forked_during_the_pick_holds_no_copy_of_the_port() {
    let stop = Arc::new(AtomicBool::new(false));
    let forker = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut forks = 0;
            while !stop.load(Ordering::Relaxed) {
                let setup = child::Setup {
                    close: (100..100_000).collect(),
                    ..child::Setup::default()
                };
                let output = child::run_program("true", &[], setup);
                assert!(output.status.success(), "{output:?}");
                forks += 1;
            }
            forks
        })
    };
    let started = Instant::now();
    let (mut picks, mut in_use) = (0, Vec::new());
    while started.elapsed() < RACE {
        let port = child::free_port();
        picks += 1;
        if let Err(err) = TcpListener::bind(("127.0.0.1", port)) {
            assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{port}: {err}");
            in_use.push(port);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let forks = forker.join().unwrap();
    eprintln!("{picks} picks, {forks} forks, ports in use at the bind: {in_use:?}");
    assert!(forks > 0 && picks > 0);
    assert_eq!(in_use, Vec::<u16>::new(), "{picks} picks, {forks} forks");
}
