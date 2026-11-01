//! `child::free_port` against the race it exists to avoid: a port picked
//! for a server must not be one the kernel can hand to any other socket
//! between the pick and the server's bind (CI, 2026-11-01: a server
//! test lost its port to another socket). The kernel hands out ports on
//! its own (`bind` to port 0, `connect`) only from the ephemeral range,
//! `net.ipv4.ip_local_port_range`, so the check holds the rest of that
//! range and asks the kernel for a port: the old pick comes up, the new
//! one cannot. One test, alone in its binary: it holds every ephemeral
//! loopback port for a moment and raises the process's descriptor limit
//! for that (skipped, with a reason, where the hard limit is too low).

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
use std::net::TcpListener;

use durchlauferhitzer::sys::rlimit::{nofile, raise_nofile};

/// `net.ipv4.ip_local_port_range`: the ephemeral ports, inclusive.
fn ephemeral() -> (u16, u16) {
    let range = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    let mut ends = range.split_whitespace().map(|end| end.parse().unwrap());
    (ends.next().unwrap(), ends.next().unwrap())
}

/// Holds every ephemeral loopback port but `port` (each bound by number;
/// one another socket holds already is skipped), then asks the kernel for
/// a port (`bind` to 0): whether it hands out `port`. With `port` outside
/// the range nothing is left to hand out.
fn kernel_hands_out(port: u16) -> bool {
    let (low, high) = ephemeral();
    let held: Vec<TcpListener> = (low..=high)
        .filter(|&p| p != port)
        .filter_map(|p| TcpListener::bind(("127.0.0.1", p)).ok())
        .collect();
    // Nearly the whole range (a few ports are held elsewhere).
    let range = usize::from(high - low) + 1;
    assert!(held.len() + 1024 >= range, "{} of {range}", held.len());
    match TcpListener::bind("127.0.0.1:0") {
        Ok(socket) => socket.local_addr().unwrap().port() == port,
        Err(_) => false,
    }
}

#[test]
fn no_other_socket_can_get_a_free_port() {
    let (low, high) = ephemeral();
    let need = u64::from(high - low) + 1 + 256;
    let (_, hard) = nofile().unwrap();
    if hard < need {
        println!(
            "skipped: exhausting the ephemeral range needs {need} descriptors, \
             the hard limit is {hard}"
        );
        return;
    }
    raise_nofile(need).unwrap();
    // The old way (bind port 0, read, release) gives an ephemeral port,
    // which the kernel hands to the next socket that asks: the control.
    let old = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert!(
        kernel_hands_out(old),
        "the old way's port {old} never came up"
    );
    for _ in 0..3 {
        let port = child::free_port();
        assert!(
            !(low..=high).contains(&port),
            "{port} is in the ephemeral range {low}-{high}"
        );
        assert!(!kernel_hands_out(port), "the kernel handed out {port}");
        // And it is still free for the server.
        TcpListener::bind(("127.0.0.1", port)).unwrap();
    }
}
