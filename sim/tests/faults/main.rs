//! Fault injection and randomized simulation: split reads, pipelining,
//! stalls, resets, EOFs and clock jumps in arbitrary combinations, with
//! static files on a random tree, slow disks and files that shrink or
//! change while they are sent.
//!
//! Proxy routes have a randomized net of their own (`proxy/`).
//!
//! Run a single random schedule of each net with `SIM_SEED=<seed>`.

#![allow(
    clippy::panic,
    reason = "a failing seed is reported by re-panicking with SIM_SEED=<seed>"
)]

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::http::target::TargetError;
use durchlauferhitzer_core::{
    ConnId, FileError, FileStamp, LimitKind, Log, RejectReason, RequestBodyError, RequestLine,
    RequestSummary, TimeoutKind,
};
use durchlauferhitzer_sim::{ConnectBehaviour, Sim, SimConnState, TimerOrder, Window};

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
host example.test {
  path / { static /srv/www; }
  path /g { static /srv/www; on_file_change allow_growth; }
  path /.well-known { static /srv/www; }
  path /@%C3%A4 { static /srv/www; on_file_change allow_growth; }
  path /api/ { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; }
}";

/// Like `RANDOM_CONFIG`, but request lines long enough for directory
/// redirects that no longer fit the send buffer (answered 414).
const LONG_RANDOM_CONFIG: &str = "listen 127.0.0.1:8080;
limits {
  max_request_line 2KiB;
  max_head 2200;
  header_timeout 10s;
  keepalive_timeout 60s;
  send_idle_timeout 30s;
  max_connections 6;
  max_connections_per_ip 3;
}
host example.test {
  path / { static /srv/www; }
  path /g { static /srv/www; on_file_change allow_growth; }
  path /.well-known { static /srv/www; }
  path /@%C3%A4 { static /srv/www; on_file_change allow_growth; }
  path /api/ { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; }
}";

/// The send buffer's capacity: 1 KiB by NFR-RES-01 (`SEND_CAPACITY` in
/// the core).
const SEND_CAPACITY: usize = 1024;

/// Unknown host: 404, keep-alive.
const VALID: &[u8] = b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n";
/// Bare LF: 400, close.
const INVALID: &[u8] = b"GET / HTTP/1.1\nHost: a\n\n";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

/// The date every simulation of this binary sets (`Sim::set_date`), so
/// both randomized nets check `Date` on every response (FR-HTTP-22a): the
/// example of RFC 9110 section 5.6.7, its text as written there.
const NET_DATE: (u64, &str) = RFC_DATE;

/// A simulation of `config` whose core has the time `NET_DATE`.
fn dated_sim(config: &str) -> Sim {
    let mut sim = Sim::new(config);
    sim.set_date(NET_DATE.0);
    sim
}

/// The timer order a seed of either net runs in: one seed in four with
/// late timers (NFR-TEST-07), chosen from the seed alone so that a failure
/// replays from `SIM_SEED`.
fn timer_order(seed: u64) -> TimerOrder {
    if seed % 4 == 0 {
        TimerOrder::Late
    } else {
        TimerOrder::OnTime
    }
}

/// A fixed response, by the model (`support/model_http.rs`), not the core's renderer.
fn fixed(status: Status, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    model_fixed(status, persistence, body, Some(NET_DATE.1))
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
    let mut sim = dated_sim(config);
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

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "../support/model_http.rs"]
mod fixed;
mod generator;
mod model;
mod oracle;
#[allow(
    clippy::indexing_slicing,
    reason = "the proxy net indexes its own generated exchanges and buffers; an out-of-range index is a bug in the net, reported with its seed like any failure"
)]
mod proxy;
mod run;
mod shutdown;
mod split;

use fixed::*;
use generator::*;
use model::*;
use oracle::*;
use run::{assert_clock_done, replay_seed, run_reporting};
use shutdown::*;
