//! Fault injection and randomized simulation: split reads, pipelining,
//! stalls, resets, EOFs and clock jumps in arbitrary combinations.
//!
//! Run a single random schedule with `SIM_SEED=<seed>`.

#![allow(
    clippy::panic,
    reason = "a failing seed is reported by re-panicking with SIM_SEED=<seed>"
)]

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::{BodyMode, render_fixed};
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{ConnId, LimitKind, Log, RejectReason, RequestSummary, TimeoutKind};
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
    let long_line = [b"GET /".as_slice(), &[b'a'; 80], b" HTTP/1.1\r\n\r\n"].concat();
    let endless_head = [
        b"GET / HTTP/1.1\r\nHost: other.test\r\nX-Pad: ".as_slice(),
        &[b'a'; 100],
    ]
    .concat();
    let cases: [(&str, &[u8], Status, Persistence); 4] = [
        (CONFIG, VALID, Status::NotFound, Persistence::KeepAlive),
        (CONFIG, INVALID, Status::BadRequest, Persistence::Close),
        (
            SMALL_HEAD,
            &long_line,
            Status::UriTooLong,
            Persistence::Close,
        ),
        (
            SMALL_HEAD,
            &endless_head,
            Status::RequestHeaderFieldsTooLarge,
            Persistence::Close,
        ),
    ];
    for (config, request, status, persistence) in cases {
        let (whole, trace) = deliver(config, &[request]);
        assert_eq!(
            whole.received,
            fixed(status, persistence, BodyMode::Full),
            "{trace}"
        );
        assert_eq!(whole.logs.len(), 1, "{trace}");
        for split in 1..request.len() {
            let (first, second) = request.split_at(split);
            let (outcome, trace) = deliver(config, &[first, second]);
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
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::Close, BodyMode::Full),
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

/// Every status the core can send; each must be seen across the seeds.
const STATUSES: [Status; 8] = [
    Status::BadRequest,
    Status::NotFound,
    Status::RequestTimeout,
    Status::UriTooLong,
    Status::RequestHeaderFieldsTooLarge,
    Status::NotImplemented,
    Status::ServiceUnavailable,
    Status::HttpVersionNotSupported,
];

/// Request templates the random clients draw from.
const REQUESTS: &[&[u8]] = &[
    // 404, keep-alive.
    b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n",
    b"GET /y HTTP/1.1\r\nHost: other.test\r\n\r\n",
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
    // Unsupported version: 505.
    b"GET / HTTP/1.2\r\nHost: other.test\r\n\r\n",
    // Over-long request line: 414.
    b"GET /aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa HTTP/1.1\r\n\r\n",
    // Head that never ends within max_head: 431.
    b"GET / HTTP/1.1\r\nHost: other.test\r\nX-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
];

/// The test's view of one simulated client.
struct Client {
    id: ConnId,
    /// Templates still to send; the front one may be partly sent.
    queue: VecDeque<Vec<u8>>,
    /// Whether the front template's first byte has been sent.
    front_started: bool,
    /// Templates whose first byte was sent, and how many were `HEAD`.
    started: usize,
    started_head: usize,
    /// Sends that carried the end of one template and the start of the next.
    pipelined_sends: usize,
    eof: bool,
    /// Everything received so far.
    received: Vec<u8>,
    /// The sim reported the connection as no longer `Open`.
    ended: bool,
}

/// What the generator reached, summed over all seeds.
#[derive(Default)]
struct Coverage {
    statuses: [usize; STATUSES.len()],
    http: usize,
    header_timeout: usize,
    send_timeout: usize,
    limit_connections: usize,
    limit_per_ip: usize,
    pipelined: usize,
}

impl Coverage {
    fn check(&self) {
        let named = [
            ("Rejected(Http)", self.http),
            ("Rejected(Timeout(Header))", self.header_timeout),
            ("Rejected(Timeout(Send))", self.send_timeout),
            ("Rejected(Limit(Connections))", self.limit_connections),
            ("Rejected(Limit(ConnectionsPerIp))", self.limit_per_ip),
            ("pipelined pairs", self.pipelined),
        ];
        for (status, n) in STATUSES.iter().zip(self.statuses) {
            assert!(n > 0, "no {} response in any seed", status.code());
        }
        for (what, n) in named {
            assert!(n > 0, "no {what} in any seed");
        }
    }
}

/// One complete received response: which fixed response it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    status: Status,
    persistence: Persistence,
    body: BodyMode,
}

/// All 32 fixed responses the core can render.
fn fixed_responses() -> Vec<(Seen, Vec<u8>)> {
    let mut out = Vec::new();
    for status in STATUSES {
        for persistence in [Persistence::KeepAlive, Persistence::Close] {
            for body in [BodyMode::Full, BodyMode::HeadOnly] {
                let seen = Seen {
                    status,
                    persistence,
                    body,
                };
                out.push((seen, fixed(status, persistence, body)));
            }
        }
    }
    out
}

/// Split `bytes` into complete fixed responses and a tail: a strict prefix
/// of some fixed response (empty unless the last write was cut short). An
/// error names the offset where nothing matches.
fn split_responses(bytes: &[u8]) -> Result<(Vec<Seen>, &[u8]), String> {
    let all = fixed_responses();
    let mut seen = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let offset = bytes.len() - rest.len();
        let full = all
            .iter()
            .filter(|(s, _)| s.body == BodyMode::Full)
            .find(|(_, r)| rest.starts_with(r));
        let head_only = all
            .iter()
            .filter(|(s, _)| s.body == BodyMode::HeadOnly)
            .find(|(_, r)| rest.starts_with(r));
        let next = match (full, head_only) {
            (Some(m), _) => Some(m),
            // A head without its body is complete only if what follows is
            // nothing or another response (bodies start with a digit).
            (None, Some(m)) => match rest.get(m.1.len()) {
                None | Some(b'H') => Some(m),
                Some(_) => None,
            },
            (None, None) => None,
        };
        match next {
            Some((s, r)) => {
                seen.push(*s);
                rest = rest.get(r.len()..).unwrap_or_default();
            }
            None => {
                return if all.iter().any(|(_, r)| r.starts_with(rest)) {
                    Ok((seen, rest))
                } else {
                    Err(format!("unrecognised response bytes at offset {offset}"))
                };
            }
        }
    }
    Ok((seen, rest))
}

/// Whether `tail` is the start of some rendering of `status`.
fn is_prefix_of(tail: &[u8], status: Status) -> bool {
    fixed_responses()
        .iter()
        .any(|(s, r)| s.status == status && r.starts_with(tail))
}

/// The status the core announced it would send with `log`, if any.
fn logged_status(log: &Log) -> Option<Status> {
    match log {
        Log::Response { status, .. } => Some(*status),
        Log::Rejected { reason, .. } => match reason {
            RejectReason::Http(e) => Some(e.status()),
            RejectReason::Timeout(TimeoutKind::Header) => Some(Status::RequestTimeout),
            RejectReason::Timeout(TimeoutKind::Send | TimeoutKind::Disk) => None,
            RejectReason::Limit(_) => Some(Status::ServiceUnavailable),
            // Not emitted in this stage; a route miss is answered 404.
            RejectReason::Route(_) => Some(Status::NotFound),
        },
        Log::Reset { .. } | Log::FileAborted { .. } => None,
    }
}

fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. } => *conn,
    }
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

/// Send a random prefix of `client`'s queued templates.
fn send_piece(sim: &mut Sim, client: &mut Client, rng: &mut fastrand::Rng) {
    if client.queue.is_empty() {
        for _ in 0..rng.usize(1..4) {
            if let Some(template) = rng.choice(REQUESTS) {
                client.queue.push_back(template.to_vec());
            }
        }
    }
    let total: usize = client.queue.iter().map(Vec::len).sum();
    let mut left = rng.usize(1..=total);
    let mut piece = Vec::new();
    let mut finished_one = false;
    while left > 0 {
        let Some(front) = client.queue.front_mut() else {
            break;
        };
        if !client.front_started {
            client.front_started = true;
            client.started += 1;
            if front.starts_with(b"HEAD ") {
                client.started_head += 1;
            }
            if finished_one {
                client.pipelined_sends += 1;
            }
        }
        let n = left.min(front.len());
        piece.extend(front.drain(..n));
        left -= n;
        if front.is_empty() {
            client.queue.pop_front();
            client.front_started = false;
            finished_one = true;
        }
    }
    sim.send(client.id, &piece);
}

/// Take what every client received; no bytes may arrive once the sim saw
/// the connection shut down or closed.
fn collect(sim: &mut Sim, clients: &mut [Client]) {
    for client in clients.iter_mut() {
        let new = sim.take_received(client.id);
        assert!(
            new.is_empty() || !client.ended,
            "conn={} received bytes after WriteShutdown/Close\ntrace:\n{}",
            client.id.0,
            sim.trace()
        );
        client.received.extend(new);
        if sim.state(client.id) != SimConnState::Open {
            client.ended = true;
        }
    }
}

/// Check one client's received bytes against its requests and logs.
fn check_client(sim: &Sim, client: &Client, coverage: &mut Coverage) {
    let fail = |what: String| -> ! {
        panic!("conn={}: {what}\ntrace:\n{}", client.id.0, sim.trace());
    };
    let (seen, tail) = match split_responses(&client.received) {
        Ok(r) => r,
        Err(e) => fail(e),
    };
    let logs: Vec<&Log> = sim
        .logs()
        .iter()
        .filter(|l| log_conn(l) == client.id)
        .collect();
    let announced: Vec<Status> = logs.iter().filter_map(|l| logged_status(l)).collect();
    // What arrived is, in order, a prefix of what the core logged.
    let statuses: Vec<Status> = seen.iter().map(|s| s.status).collect();
    if !announced.starts_with(&statuses) {
        fail(format!("received {statuses:?} but logged {announced:?}"));
    }
    let truncated = !tail.is_empty();
    if truncated {
        match announced.get(statuses.len()) {
            Some(status) if is_prefix_of(tail, *status) => {}
            next => fail(format!(
                "cut-short response after {statuses:?} does not start {next:?}"
            )),
        }
    }
    // Less than announced, or cut short, only when the write was abandoned
    // (send timeout or reset) or a refusal was closed at once.
    if truncated || statuses.len() < announced.len() {
        let abandoned = matches!(
            logs.last(),
            Some(Log::Rejected {
                reason: RejectReason::Timeout(TimeoutKind::Send | TimeoutKind::Disk),
                ..
            }) | Some(Log::Reset { .. })
                | Some(Log::FileAborted { .. })
        );
        let refused_at_once = statuses.is_empty()
            && matches!(
                logs.as_slice(),
                [Log::Rejected {
                    reason: RejectReason::Limit(_),
                    ..
                }]
            );
        if !abandoned && !refused_at_once {
            fail(format!(
                "received {statuses:?} (truncated: {truncated}) of {announced:?}"
            ));
        }
    }
    let refused = statuses.contains(&Status::ServiceUnavailable);
    if refused && statuses.len() != 1 {
        fail(format!("503 is not the only response: {statuses:?}"));
    }
    let answered = statuses.len() - usize::from(refused);
    if answered > client.started {
        fail(format!(
            "{answered} responses to {} requests started",
            client.started
        ));
    }
    let head_only = seen.iter().filter(|s| s.body == BodyMode::HeadOnly).count();
    if head_only > client.started_head {
        fail(format!(
            "{head_only} bodiless responses to {} HEAD requests",
            client.started_head
        ));
    }
    if let Some(close) = seen
        .iter()
        .position(|s| s.persistence == Persistence::Close)
    {
        if close + 1 != seen.len() || truncated {
            fail(format!("response after Connection: close: {seen:?}"));
        }
    }
    for (slot, status) in coverage.statuses.iter_mut().zip(STATUSES) {
        *slot += statuses.iter().filter(|s| **s == status).count();
    }
    if client.pipelined_sends > 0 && answered >= 2 {
        coverage.pipelined += 1;
    }
}

/// One random schedule. Panics (sim invariant or check) on failure.
fn run_seed(seed: u64, coverage: &mut Coverage) {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut sim = Sim::new(RANDOM_CONFIG);
    let mut clients: Vec<Client> = Vec::new();
    let steps = rng.usize(5..80);
    for step in 0..steps {
        let op = rng.u8(0..100);
        if op < 25 || step < 3 || clients.is_empty() {
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
                queue: VecDeque::new(),
                front_started: false,
                started: 0,
                started_head: 0,
                pipelined_sends: 0,
                eof: false,
                received: Vec::new(),
                ended: false,
            });
        } else {
            let i = rng.usize(0..clients.len());
            let Some(client) = clients.get_mut(i) else {
                continue;
            };
            match op {
                25..60 => {
                    if !client.eof {
                        send_piece(&mut sim, client, &mut rng);
                    }
                }
                60..68 => sim.set_client_window(client.id, random_window(&mut rng)),
                68..72 => sim.reset(client.id),
                72..78 => {
                    sim.send_eof(client.id);
                    client.eof = true;
                }
                78..86 => sim.advance(Duration::from_millis(rng.u64(0..12_000))),
                86..90 => sim.advance(Duration::from_secs(rng.u64(20..=90))),
                _ => sim.set_read_chunk(client.id, rng.usize(1..200)),
            }
        }
        if rng.u8(0..4) != 0 {
            sim.run_until_idle();
        }
        collect(&mut sim, &mut clients);
    }
    // Wind down: every client sends EOF, then 5 minutes pass.
    for client in &clients {
        sim.send_eof(client.id);
    }
    sim.run_until_idle();
    collect(&mut sim, &mut clients);
    sim.advance(Duration::from_secs(300));
    collect(&mut sim, &mut clients);
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
    for client in &clients {
        check_client(&sim, client, coverage);
    }
    for log in sim.logs() {
        if let Log::Rejected { reason, .. } = log {
            match reason {
                RejectReason::Http(_) => coverage.http += 1,
                RejectReason::Timeout(TimeoutKind::Header) => coverage.header_timeout += 1,
                RejectReason::Timeout(TimeoutKind::Send) => coverage.send_timeout += 1,
                // Static files join the random runs in a later task.
                RejectReason::Timeout(TimeoutKind::Disk) => {}
                RejectReason::Limit(LimitKind::Connections) => coverage.limit_connections += 1,
                RejectReason::Limit(LimitKind::ConnectionsPerIp) => coverage.limit_per_ip += 1,
                RejectReason::Route(_) => {}
            }
        }
    }
}

/// Run one seed; on failure, panic with `SIM_SEED=<seed>` and the original
/// message (which carries the trace).
fn run_seed_reporting(seed: u64, coverage: &mut Coverage) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| run_seed(seed, coverage))) {
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
    let mut coverage = Coverage::default();
    match std::env::var("SIM_SEED") {
        Ok(seed) => match seed.parse::<u64>() {
            Ok(seed) => run_seed_reporting(seed, &mut coverage),
            Err(e) => panic!("SIM_SEED={seed:?} is not a u64: {e}"),
        },
        Err(_) => {
            for seed in 0..SEEDS {
                run_seed_reporting(seed, &mut coverage);
            }
            eprintln!(
                "COVERAGE {:?} http={} header={} send={} conns={} per_ip={} pipelined={}",
                coverage.statuses,
                coverage.http,
                coverage.header_timeout,
                coverage.send_timeout,
                coverage.limit_connections,
                coverage.limit_per_ip,
                coverage.pipelined
            );
            coverage.check();
        }
    }
}
