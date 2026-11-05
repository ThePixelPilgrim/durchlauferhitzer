//! Loopback ports for the tests, below the ephemeral range, shared by
//! `child.rs` and `sys.rs`: no socket can get one from the kernel,
//! neither a listener bound to port 0 nor a connect's source port. So a
//! refused port is one of these too: a connect to an ephemeral port that
//! nothing listens on can get that port as its own source and connect to
//! itself.
//!
//! The pick probes with a listener, then closes it. A child forked by
//! another test thread at that moment holds a copy of the probe until its
//! `exec`, so for that short while the port still listens: binding it
//! again fails with `EADDRINUSE`, and a connect to it succeeds. A server
//! binds its port only after its own `exec`, much later; a test that binds
//! the port itself takes [`free_listener`], and one that needs a refused
//! port takes [`refused_port`].

use std::fs;
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// A loopback listener on a port below the ephemeral range, a new port
/// per pick within the process (`free_port.rs`).
pub fn free_listener() -> TcpListener {
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
    for _ in 0..span {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let port = u16::try_from(low + start.wrapping_add(n) % span).unwrap();
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return listener;
        }
    }
    panic!("no free port in {low}-{}", low + span - 1);
}

/// A free loopback port for a server that no other socket can be given
/// before the server binds it ([`free_listener`]'s, closed). Only another
/// program binding that very port could take it.
pub fn free_port() -> u16 {
    free_listener().local_addr().unwrap().port()
}

/// A free port (as [`free_port`]) that a connect is refused on: it waits
/// out a forked copy of the probe (at most 5 s).
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
