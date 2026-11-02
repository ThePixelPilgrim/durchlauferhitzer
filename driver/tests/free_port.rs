//! `child::free_port` against the race it exists to avoid: a port picked
//! for a server must not be one the kernel can hand to any other socket
//! between the pick and the server's bind (CI, 2026-11-01: a server
//! test lost its port to another socket). The kernel hands out ports on
//! its own (`bind` to port 0, `connect`) only from the ephemeral range,
//! `net.ipv4.ip_local_port_range`. A sample of the ports it hands out
//! lies in that range; `free_port`'s lie outside it.

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
use std::net::{TcpListener, TcpStream};

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
