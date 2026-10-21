//! Connection lifecycle through the simulation driver.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use durchlauferhitzer_core::{Interest, Log};
use durchlauferhitzer_sim::{Sim, SimConnState};

const CONFIG: &str = "listen 127.0.0.1:8080; host a.test { path / { static /srv; } }";
const SMALL_BUFFERS: &str = "listen 127.0.0.1:8080; \
    limits { max_request_line 64; max_head 64; } \
    host a.test { path / { static /srv; } }";

fn peer(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
}

#[test]
fn accepted_connection_reads() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
    assert!(sim.logs().is_empty(), "{}", sim.trace());
}

#[test]
fn client_reset_closes_and_logs() {
    let mut sim = Sim::new(CONFIG);
    let p = peer(1);
    let c = sim.connect(p);
    sim.run_until_idle();
    sim.reset(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Reset { conn: c, peer: p }],
        "{}",
        sim.trace()
    );
}

#[test]
fn client_eof_closes_silently() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.logs().is_empty(), "{}", sim.trace());
}

#[test]
fn receive_buffer_applies_backpressure() {
    let mut sim = Sim::new(SMALL_BUFFERS);
    let c = sim.connect(peer(1));
    sim.send(c, &[b'x'; 100]);
    sim.run_until_idle();
    assert_eq!(sim.queued_to_server(c), 34, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::None, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn read_chunk_limits_each_delivery() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer(1));
    sim.set_read_chunk(c, 1);
    sim.send(c, b"0123456789");
    sim.run_until_idle();
    let received: Vec<&str> = sim
        .trace()
        .lines()
        .filter(|l| l.contains(" Received "))
        .collect();
    assert_eq!(received.len(), 10, "{}", sim.trace());
    assert!(
        received.iter().all(|l| l.ends_with(" Received len=1")),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.queued_to_server(c), 0, "{}", sim.trace());
}

#[test]
fn connections_are_independent() {
    let mut sim = Sim::new(CONFIG);
    let a = sim.connect(peer(1));
    let b = sim.connect(peer(2));
    sim.run_until_idle();
    sim.reset(a);
    sim.run_until_idle();
    assert_eq!(sim.state(a), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.state(b), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(b), Interest::Read, "{}", sim.trace());
}

#[test]
fn advance_moves_the_clock() {
    let mut sim = Sim::new(CONFIG);
    let start = sim.now();
    sim.advance(Duration::from_secs(5));
    assert_eq!(sim.now(), start.saturating_add(Duration::from_secs(5)));
}
