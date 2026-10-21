//! Fault injection and randomized simulation: split reads, pipelining,
//! stalls, resets, EOFs and clock jumps in arbitrary combinations.
//!
//! Run a single random schedule with `SIM_SEED=<seed>`.

#![allow(
    clippy::panic,
    reason = "a failing seed is reported by re-panicking with SIM_SEED=<seed>"
)]

use std::net::{IpAddr, Ipv4Addr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::{BodyMode, render_fixed};
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{ConnId, Log, RejectReason, RequestSummary};
use durchlauferhitzer_sim::{Sim, SimConnState, Window};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test { path / { static /srv/www; } }";

/// Capacity 128 = max_head.
const SMALL_HEAD: &str = "listen 127.0.0.1:8080;
limits { max_request_line 64; max_head 128; }
host example.test { path / { static /srv/www; } }";

/// Small caps and buffers so the random schedules hit every limit.
const RANDOM_CONFIG: &str = "listen 127.0.0.1:8080;
limits {
  max_request_line 64;
  max_head 160;
  header_timeout 10s;
  keepalive_timeout 60s;
  send_idle_timeout 30s;
  max_connections 6;
  max_connections_per_ip 3;
}
host example.test { path / { static /srv/www; } path /api/ { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; } }";

/// Unknown host: 404, keep-alive.
const VALID: &[u8] = b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n";
/// Bare LF: 400, close.
const INVALID: &[u8] = b"GET / HTTP/1.1\nHost: a\n\n";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn fixed(status: Status, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    let mut out = Vec::new();
    render_fixed(status, persistence, body, &mut out);
    out
}

fn not_found_keep_alive() -> Vec<u8> {
    fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full)
}

fn response_log(conn: ConnId, host: Option<&str>, path: &str) -> Log {
    Log::Response {
        conn,
        peer: peer(),
        status: Status::NotFound,
        request: Some(RequestSummary {
            method: Method::Get,
            host: host.map(str::to_owned),
            path: path.to_owned(),
        }),
    }
}

/// What a client observes on one connection.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    received: Vec<u8>,
    logs: Vec<Log>,
    state: SimConnState,
}

/// Deliver `parts` in separate pumps on a fresh connection.
fn deliver(config: &str, parts: &[&[u8]]) -> (Outcome, String) {
    let mut sim = Sim::new(config);
    let c = sim.connect(peer());
    for part in parts {
        sim.send(c, part);
        sim.run_until_idle();
    }
    let outcome = Outcome {
        received: sim.take_received(c),
        logs: sim.logs().to_vec(),
        state: sim.state(c),
    };
    (outcome, sim.trace().to_owned())
}

#[test]
fn every_split_point_gives_identical_responses() {
    for request in [VALID, INVALID] {
        let (whole, _) = deliver(CONFIG, &[request]);
        assert!(!whole.received.is_empty());
        for split in 1..request.len() {
            let (first, second) = request.split_at(split);
            let (outcome, trace) = deliver(CONFIG, &[first, second]);
            assert_eq!(outcome, whole, "split at {split}\n{trace}");
        }
    }
}

#[test]
fn byte_by_byte_delivery() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.set_read_chunk(c, 1);
    sim.send(c, b"GET /a HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.send(c, b"GET /b HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    let mut expected = not_found_keep_alive();
    expected.extend(not_found_keep_alive());
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Some("other.test"), "/a"),
            response_log(c, Some("other.test"), "/b"),
        ],
        "{}",
        sim.trace()
    );
}

#[test]
fn pipelined_requests_in_one_read() {
    let a: &[u8] = b"GET /a HTTP/1.1\r\nHost: other.test\r\n\r\n";
    let b: &[u8] = b"HEAD /b HTTP/1.1\r\nHost: other.test\r\n\r\n";
    let (sequential, _) = deliver(CONFIG, &[a, b]);
    let both = [a, b].concat();
    let (pipelined, trace) = deliver(CONFIG, &[&both]);
    assert_eq!(pipelined, sequential, "{trace}");
    assert_eq!(pipelined.logs.len(), 2, "{trace}");
    assert_eq!(pipelined.state, SimConnState::Open, "{trace}");
}

#[test]
fn pipelined_bytes_beyond_head_limit() {
    let mut sim = Sim::new(SMALL_HEAD);
    let c = sim.connect(peer());
    let mut bytes = VALID.to_vec();
    bytes.extend_from_slice(b"GET /y HTTP/1.1\r\nHost: other.test\r\nX-Pad: ");
    bytes.extend_from_slice(&[b'a'; 200]);
    sim.send(c, &bytes);
    sim.run_until_idle();
    let mut expected = not_found_keep_alive();
    expected.extend(fixed(
        Status::RequestHeaderFieldsTooLarge,
        Persistence::Close,
        BodyMode::Full,
    ));
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Some("other.test"), "/x"),
            Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Http(HttpError::HeadTooLarge),
            },
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

#[test]
fn no_answer_after_connection_close_pipelined() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.send(
        c,
        b"GET /x HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n\
          GET /y HTTP/1.1\r\nHost: other.test\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[response_log(c, Some("other.test"), "/x")],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    sim.send_eof(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
}

#[test]
fn partial_pipelined_leftover_completed_later() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    let mut bytes = VALID.to_vec();
    bytes.extend_from_slice(b"GET /y HTTP/1.1\r\nHo");
    sim.send(c, &bytes);
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.logs().len(), 1, "{}", sim.trace());
    sim.send(c, b"st: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        not_found_keep_alive(),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Some("other.test"), "/x"),
            response_log(c, Some("other.test"), "/y"),
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
}

#[test]
fn http10_request_closes_after_response() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(peer());
    sim.send(c, b"GET /x HTTP/1.0\r\n\r\n");
    sim.run_until_idle();
    let received = sim.take_received(c);
    assert_eq!(
        received,
        fixed(Status::NotFound, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert!(
        received
            .windows(b"Connection: close\r\n".len())
            .any(|w| w == b"Connection: close\r\n"),
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        &[response_log(c, None, "/x")],
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
}

// ---------------------------------------------------------------------------
// Randomized fault schedules

const SEEDS: u64 = 500;

/// Request templates the random clients draw from.
const REQUESTS: &[&[u8]] = &[
    // 404, keep-alive.
    b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n",
    b"HEAD / HTTP/1.1\r\nHost: other.test\r\n\r\n",
    // Matched route: 501, close.
    b"GET /api/v HTTP/1.1\r\nHost: example.test\r\n\r\n",
    // Explicit close and HTTP/1.0.
    b"GET /c HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n",
    b"GET /old HTTP/1.0\r\n\r\n",
    // Unread body: close after the response.
    b"POST /b HTTP/1.1\r\nHost: other.test\r\nContent-Length: 5\r\n\r\nhello",
    // Malformed: 400.
    b"GET / HTTP/1.1\nHost: a\n\n",
    b"\x00\xffgarbage\r\n\r\n",
    // Over-long request line: 414.
    b"GET /aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa HTTP/1.1\r\n\r\n",
    // Head that never ends within max_head: 431.
    b"GET / HTTP/1.1\r\nHost: other.test\r\nX-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
];

/// The test's view of one simulated client.
struct Client {
    id: ConnId,
    /// Bytes this client still means to send.
    outgoing: Vec<u8>,
    eof: bool,
}

fn random_window(rng: &mut fastrand::Rng) -> Window {
    match rng.u8(0..5) {
        0 => Window::Unlimited,
        1 => Window::Bytes(rng.usize(1..64)),
        2 => Window::Stalled,
        3 => Window::Budget(rng.usize(0..200)),
        _ => Window::Bytes(1),
    }
}

/// One random schedule. Panics (sim invariant or final assertion) on failure.
fn run_seed(seed: u64) {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut sim = Sim::new(RANDOM_CONFIG);
    let mut clients: Vec<Client> = Vec::new();
    let steps = rng.usize(5..60);
    for _ in 0..steps {
        let op = rng.u8(0..10);
        if op == 0 || clients.is_empty() {
            let ip = IpAddr::V4(Ipv4Addr::new(192, 0, 2, rng.u8(1..4)));
            let id = sim.connect(ip);
            if rng.bool() {
                sim.set_read_chunk(id, rng.usize(1..40));
            }
            if rng.u8(0..3) == 0 {
                sim.set_client_window(id, random_window(&mut rng));
            }
            clients.push(Client {
                id,
                outgoing: Vec::new(),
                eof: false,
            });
            continue;
        }
        let i = rng.usize(0..clients.len());
        let Some(client) = clients.get_mut(i) else {
            continue;
        };
        match op {
            1..=4 => {
                if client.eof {
                    continue;
                }
                if client.outgoing.is_empty() {
                    for _ in 0..rng.usize(1..4) {
                        if let Some(template) = rng.choice(REQUESTS) {
                            client.outgoing.extend_from_slice(template);
                        }
                    }
                }
                let n = rng.usize(1..=client.outgoing.len());
                let piece: Vec<u8> = client.outgoing.drain(..n).collect();
                sim.send(client.id, &piece);
            }
            5 => sim.set_client_window(client.id, random_window(&mut rng)),
            6 => {
                if rng.u8(0..3) == 0 {
                    sim.reset(client.id);
                }
            }
            7 => {
                sim.send_eof(client.id);
                client.eof = true;
            }
            8 => sim.advance(Duration::from_millis(rng.u64(0..15_000))),
            _ => sim.set_read_chunk(client.id, rng.usize(1..200)),
        }
        if rng.u8(0..4) != 0 {
            sim.run_until_idle();
        }
    }
    // Wind down: every client sends EOF, then 5 minutes pass.
    for client in &clients {
        sim.send_eof(client.id);
    }
    sim.run_until_idle();
    sim.advance(Duration::from_secs(300));
    for id in sim.conn_ids() {
        assert_eq!(
            sim.state(id),
            SimConnState::Closed,
            "conn={} not closed\ntrace:\n{}",
            id.0,
            sim.trace()
        );
        assert!(
            !sim.core_knows(id),
            "core still holds conn={}\ntrace:\n{}",
            id.0,
            sim.trace()
        );
    }
}

/// Run one seed; on failure, panic with `SIM_SEED=<seed>` and the original
/// message (which carries the trace).
fn run_seed_reporting(seed: u64) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| run_seed(seed))) {
        let message = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        panic!("SIM_SEED={seed} failed: {message}");
    }
}

#[test]
fn random_fault_schedules() {
    match std::env::var("SIM_SEED") {
        Ok(seed) => match seed.parse::<u64>() {
            Ok(seed) => run_seed_reporting(seed),
            Err(e) => panic!("SIM_SEED={seed:?} is not a u64: {e}"),
        },
        Err(_) => {
            for seed in 0..SEEDS {
                run_seed_reporting(seed);
            }
        }
    }
}
