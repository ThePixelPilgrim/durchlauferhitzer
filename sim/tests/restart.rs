//! The restart drain in the core (FR-PROC-07, FR-PROC-05 target restart),
//! driven through the simulation: what the old process holds is served,
//! each answer saying `Connection: close`, and nothing is refused or
//! closed early. The shutdown drain's scenarios are in `shutdown.rs`.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use durchlauferhitzer_core::http::request::Persistence;
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{DrainTarget, Log};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "support/model_http.rs"]
mod model_http;

use model_http::{model_file_head, model_fixed};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test { path / { static /srv/www; } }";

const GET_F: &[u8] = b"GET /f.txt HTTP/1.1\r\nHost: example.test\r\n\r\n";
const MISSING: &[u8] = b"GET /nope HTTP/1.1\r\nHost: example.test\r\n\r\n";
const TEXT: &str = "text/plain; charset=utf-8";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn file(persistence: Persistence) -> Vec<u8> {
    let mut bytes = model_file_head(TEXT, 3000, persistence, None);
    bytes.extend_from_slice(&[b'x'; 3000]);
    bytes
}

fn not_found(persistence: Persistence) -> Vec<u8> {
    model_fixed(Status::NotFound, persistence, BodyMode::Full, None)
}

/// Across a restart's drain: a download in flight (its head queued
/// keep-alive) with a request pipelined behind it, two idle keep-alive
/// connections and one accepted after the drain began. The download
/// finishes as announced and the pipelined request gets its own 200 with
/// close; the idle one that asks again gets its 404 with close; the
/// late one is served with close; the other idle one ends at its
/// keep-alive deadline. No 503 anywhere, then the core is drained.
#[test]
fn a_restart_drain_serves_what_it_holds_with_close() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "f.txt", &[b'x'; 3000]);
    let busy = sim.connect(peer());
    sim.set_client_window(busy, Window::Budget(1000));
    sim.send(busy, &[GET_F, GET_F].concat());
    let idle = sim.connect(peer());
    sim.send(idle, MISSING);
    let quiet = sim.connect(peer());
    sim.send(quiet, MISSING);
    sim.run_until_idle();
    assert_eq!(sim.take_received(idle), not_found(Persistence::KeepAlive));
    assert_eq!(sim.take_received(quiet), not_found(Persistence::KeepAlive));

    let logs = sim.logs().len();
    sim.begin_drain(DrainTarget::Restart);
    for conn in [busy, idle, quiet] {
        assert_eq!(sim.state(conn), SimConnState::Open, "{}", sim.trace());
    }
    assert_eq!(sim.logs().len(), logs, "{}", sim.trace());

    sim.set_client_window(busy, Window::Unlimited);
    sim.run_until_idle();
    let want = [file(Persistence::KeepAlive), file(Persistence::Close)].concat();
    assert_eq!(sim.take_received(busy), want, "{}", sim.trace());
    assert_eq!(
        sim.state(busy),
        SimConnState::WriteShutdown,
        "{}",
        sim.trace()
    );

    sim.send(idle, MISSING);
    sim.run_until_idle();
    assert_eq!(sim.take_received(idle), not_found(Persistence::Close));
    assert_eq!(
        sim.state(idle),
        SimConnState::WriteShutdown,
        "{}",
        sim.trace()
    );

    let late = sim.connect(peer());
    sim.send(late, GET_F);
    sim.run_until_idle();
    assert_eq!(sim.take_received(late), file(Persistence::Close));
    assert_eq!(
        sim.state(late),
        SimConnState::WriteShutdown,
        "{}",
        sim.trace()
    );

    assert!(
        sim.logs().iter().all(|l| !matches!(
            l,
            Log::Rejected { .. }
                | Log::Response {
                    status: Status::ServiceUnavailable,
                    ..
                }
        )),
        "{}",
        sim.trace()
    );
    for conn in [busy, idle, late] {
        sim.send_eof(conn);
    }
    sim.run_until_idle();
    // The quiet one is kept until its keep-alive deadline (60 s).
    sim.advance(Duration::from_secs(59));
    assert_eq!(sim.state(quiet), SimConnState::Open, "{}", sim.trace());
    assert!(!sim.is_drained());
    sim.advance(Duration::from_secs(1));
    assert_eq!(sim.state(quiet), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.is_drained(), "{}", sim.trace());
    assert_eq!(sim.take_received(quiet), b"");
}
