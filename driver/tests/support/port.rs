//! A loopback port for the tests, below the ephemeral range
//! (`free_port`), shared by `child.rs` and `sys.rs`: no socket can get
//! it from the kernel, neither a listener bound to port 0 nor a connect's
//! source port (which is why a refused port is one of these too: a
//! connect to an ephemeral port nothing listens on can connect to
//! itself).

use std::fs;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};

/// A free loopback port for a server that no other socket can be given
/// before the server binds it: the kernel hands out ports itself (`bind`
/// to 0, `connect`) only from `net.ipv4.ip_local_port_range`, so it is
/// one of the 16384 below it, a new one per pick (`free_port.rs`). Only
/// another program binding that very port could take it.
pub fn free_port() -> u16 {
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
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
    panic!("no free port in {low}-{}", low + span - 1);
}
