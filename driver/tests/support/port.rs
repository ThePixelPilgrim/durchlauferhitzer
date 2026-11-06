//! Loopback ports for the tests, below the ephemeral range, shared by
//! `child.rs` and `sys.rs`: no socket can get one from the kernel,
//! neither a listener bound to port 0 nor a connect's source port. So a
//! refused port is one of these too: a connect to an ephemeral port that
//! nothing listens on can get that port as its own source and connect to
//! itself.
//!
//! [`free_port`] opens no socket: it takes the first candidate that no
//! TCP socket of the network namespace uses (`/proc/net/tcp` and
//! `tcp6`, any state). A probe listener would not do: a child forked by
//! another test thread holds a copy of every descriptor of the test
//! process until its `exec` (`O_CLOEXEC` closes them only there, and the
//! tests' `pre_exec` hooks stretch that window), so the closed probe would
//! still listen and the server's own bind fail with `EADDRINUSE` (CI,
//! arm64, 2026-11-06; `free_port.rs` reproduces it). A test that binds
//! the port itself takes [`free_listener`]; one that needs a refused port
//! takes [`refused_port`].

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The next candidate ports, below the ephemeral range, a new one per
/// call within the process (`free_port.rs`), wrapping around.
fn candidates() -> impl Iterator<Item = u16> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let range = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    let ephemeral: u32 = range.split_whitespace().next().unwrap().parse().unwrap();
    let low = ephemeral.saturating_sub(16384).max(1024);
    let span = ephemeral.saturating_sub(low);
    assert!(
        span >= 1024,
        "the ephemeral ports start at {ephemeral}: no room below them for test ports"
    );
    let start = std::process::id().wrapping_mul(7919) % span;
    (0..span).map(move |_| {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        u16::try_from(low + start.wrapping_add(n) % span).unwrap()
    })
}

/// A loopback listener on a port below the ephemeral range.
#[allow(dead_code, reason = "each test binary uses part")]
pub fn free_listener() -> TcpListener {
    candidates()
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("no free port below the ephemeral range")
}

/// The local ports of every TCP socket of the network namespace, in any
/// state (`/proc/net/tcp` and `tcp6`).
fn ports_in_use() -> HashSet<u16> {
    let mut ports = HashSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let port = line
                .split_whitespace()
                .nth(1)
                .and_then(|local| local.rsplit_once(':'))
                .and_then(|(_, port)| u16::from_str_radix(port, 16).ok());
            ports.extend(port);
        }
    }
    ports
}

/// A free loopback port for a server, below the ephemeral range (so the
/// kernel gives it to no other socket) and used by no TCP socket now,
/// picked without opening one. Only another program binding that very
/// port could take it.
pub fn free_port() -> u16 {
    let in_use = ports_in_use();
    candidates()
        .find(|port| !in_use.contains(port))
        .expect("no free port below the ephemeral range")
}

/// A free port (as [`free_port`]) that a connect is refused on (checked,
/// at most 5 s).
pub fn refused_port() -> u16 {
    let port = free_port();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Err(err) if err.kind() == ErrorKind::ConnectionRefused => return port,
            Err(err) => panic!("connect to 127.0.0.1:{port}: {err}"),
            Ok(_) => {
                assert!(Instant::now() < deadline, "127.0.0.1:{port} still listens");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
