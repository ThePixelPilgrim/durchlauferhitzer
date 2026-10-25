//! Fault injection and randomized simulation: split reads, pipelining,
//! stalls, resets, EOFs and clock jumps in arbitrary combinations, with
//! static files on a random tree, slow disks and files that shrink or
//! change while they are sent.
//!
//! Run a single random schedule with `SIM_SEED=<seed>`.

#![allow(
    clippy::panic,
    reason = "a failing seed is reported by re-panicking with SIM_SEED=<seed>"
)]

use std::collections::{BTreeMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::{
    BodyMode, render_file_head, render_fixed, render_redirect,
};
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::{
    ConnId, FileError, FileStamp, LimitKind, Log, RejectReason, RequestSummary, TimeoutKind,
};
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
host example.test {
  path / { static /srv/www; }
  path /g { static /srv/www; on_file_change allow_growth; }
  path /api/ { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; }
}";

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

const SEEDS: u64 = 1500;

/// Every status the core can send; each must be seen across the seeds.
const STATUSES: [Status; 12] = [
    Status::Ok,
    Status::MovedPermanently,
    Status::BadRequest,
    Status::NotFound,
    Status::MethodNotAllowed,
    Status::RequestTimeout,
    Status::ContentTooLarge,
    Status::UriTooLong,
    Status::RequestHeaderFieldsTooLarge,
    Status::NotImplemented,
    Status::ServiceUnavailable,
    Status::HttpVersionNotSupported,
];

/// Request templates without static files that the random clients draw
/// from.
const REQUESTS: &[&[u8]] = &[
    // 404, keep-alive.
    b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n",
    b"GET /y HTTP/1.1\r\nHost: other.test\r\n\r\n",
    b"HEAD / HTTP/1.1\r\nHost: other.test\r\n\r\n",
    // Matched proxy route: 501, close.
    b"GET /api/v HTTP/1.1\r\nHost: example.test\r\n\r\n",
    b"HEAD /api/v HTTP/1.1\r\nHost: example.test\r\n\r\n",
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

/// Paths (relative to a static route) the random clients ask for: files,
/// directories with and without `index.html` (with and without a slash),
/// missing paths, dotfiles, symlinks, a denied file and directory, a
/// special file and a file asked for as a directory.
const STATIC_PATHS: &[&str] = &[
    "",
    "index.html",
    "a.txt",
    "b.bin",
    "c.css",
    "big.bin",
    "d",
    "d/",
    "d/index.html",
    "d/e.txt",
    "f",
    "f/",
    "f/x.bin",
    "missing.txt",
    "nodir/",
    ".env",
    "d/.hidden",
    "s.txt",
    "ln/x",
    "secret.txt",
    "priv/x.txt",
    "fifo",
    "a.txt/",
];

/// The static root both static routes serve.
const ROOT: &str = "/srv/www";
/// How much one `Readahead` covers (the core's chunk size).
const CHUNK: u64 = 256 * 1024;
const MIB: u64 = 1024 * 1024;

/// A static request: a path under `/` (abort on change) or `/g` (allow
/// growth). Bodies only go to `/up/` paths, so the expected status of
/// every logged request follows from its method and path alone.
fn random_static_request(rng: &mut fastrand::Rng) -> Vec<u8> {
    let rel = rng.choice(STATIC_PATHS).copied().unwrap_or_default();
    let prefix = if rng.bool() { "/" } else { "/g/" };
    let path = match (rel, prefix) {
        ("", "/g/") if rng.bool() => "/g".to_owned(),
        _ => format!("{prefix}{rel}"),
    };
    let host = "Host: example.test\r\n";
    match rng.u8(0..20) {
        0..10 => format!("GET {path} HTTP/1.1\r\n{host}\r\n").into_bytes(),
        10..15 => format!("HEAD {path} HTTP/1.1\r\n{host}\r\n").into_bytes(),
        15 => format!("POST {path} HTTP/1.1\r\n{host}\r\n").into_bytes(),
        16 => format!("POST {path} HTTP/1.1\r\n{host}Content-Length: 2\r\n\r\nhi").into_bytes(),
        17 | 18 => format!("GET {prefix}up/{rel} HTTP/1.1\r\n{host}Content-Length: 3\r\n\r\nabc")
            .into_bytes(),
        _ => format!("HEAD {prefix}up/{rel} HTTP/1.1\r\n{host}Content-Length: 1\r\n\r\nx")
            .into_bytes(),
    }
}

fn random_request(rng: &mut fastrand::Rng) -> Vec<u8> {
    if rng.u8(0..10) < 7 {
        random_static_request(rng)
    } else {
        rng.choice(REQUESTS).map(|r| r.to_vec()).unwrap_or_default()
    }
}

/// The change policy of the static route a path falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Abort,
    AllowGrowth,
}

/// A fault injected into every transfer of a file.
#[derive(Debug, Clone, Copy)]
enum Fault {
    None,
    /// `sendfile` hits end of file at this offset (below the size).
    Shrink(u64),
    /// Chunks read after `after` chunks report the stamp `new`.
    Change {
        after: u64,
        new: FileStamp,
    },
}

#[derive(Debug)]
struct ModelFile {
    bytes: Vec<u8>,
    content_type: &'static str,
    fault: Fault,
}

impl ModelFile {
    fn size(&self) -> u64 {
        self.bytes.len() as u64
    }

    /// How a `GET` of this file under `policy` must end if nothing else
    /// (a timeout or reset) cuts it first: `None` when the whole body is
    /// sent, otherwise the abort error and the body bytes received at the
    /// cut (`None` when the cut comes before the head is necessarily out).
    fn expected_abort(&self, policy: Policy) -> Option<(FileError, Option<u64>)> {
        let size = self.size();
        match self.fault {
            Fault::None => None,
            Fault::Shrink(at) => Some((FileError::Truncated, Some(at))),
            Fault::Change { after, new } => {
                if after >= size.div_ceil(CHUNK) {
                    return None;
                }
                let error = match policy {
                    Policy::Abort => FileError::Changed,
                    Policy::AllowGrowth => match new.size.cmp(&size) {
                        std::cmp::Ordering::Greater => return None,
                        std::cmp::Ordering::Equal => FileError::Changed,
                        std::cmp::Ordering::Less => FileError::Truncated,
                    },
                };
                let at = (after > 0).then_some(after * CHUNK);
                Some((error, at))
            }
        }
    }
}

#[derive(Debug)]
enum Node {
    File(ModelFile),
    Dir,
    /// A symlink, a denied entry or a special file: never served, and
    /// nothing beneath it is reachable.
    Unservable,
}

/// What a static `GET` or `HEAD` without a body must be answered with.
enum Expect<'t> {
    File(&'t ModelFile, Policy),
    Redirect,
    NotFound,
}

/// The test's model of the random file tree under `ROOT`.
#[derive(Debug, Default)]
struct Tree {
    /// Keyed by the relative path, e.g. `d/e.txt`.
    nodes: BTreeMap<String, Node>,
}

fn content_type_of(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some(_) | None => "application/octet-stream",
    }
}

/// File sizes from 0 B to 1 MiB, with the chunk boundaries; small seeds
/// keep to small files so tiny client windows stay cheap.
fn random_size(rng: &mut fastrand::Rng, big: bool) -> u64 {
    if big {
        match rng.u8(0..8) {
            0 => 0,
            1 => CHUNK - 1,
            2 => CHUNK,
            3 => CHUNK + 1,
            4 => 2 * CHUNK,
            5 => MIB,
            _ => rng.u64(0..=MIB),
        }
    } else if rng.u8(0..6) == 0 {
        0
    } else {
        rng.u64(1..1500)
    }
}

fn random_fault(rng: &mut fastrand::Rng, size: u64) -> Fault {
    if size == 0 {
        return Fault::None;
    }
    match rng.u8(0..4) {
        0 => Fault::Shrink(rng.u64(0..size)),
        1 => {
            // `after` may equal the chunk count: the change is never seen.
            let after = rng.u64(0..=size.div_ceil(CHUNK));
            let new_size = match rng.u8(0..3) {
                0 => size + rng.u64(1..1000),
                1 => size,
                _ => rng.u64(0..size),
            };
            // Far above the sim's own modification times.
            let new = FileStamp {
                size: new_size,
                modified_ns: 1_000_000 + rng.u64(0..1000),
            };
            Fault::Change { after, new }
        }
        _ => Fault::None,
    }
}

impl Tree {
    fn add_parents(&mut self, path: &str) {
        for (i, _) in path.match_indices('/') {
            if let Some(parent) = path.get(..i) {
                self.nodes.entry(parent.to_owned()).or_insert(Node::Dir);
            }
        }
    }

    fn add_dir(&mut self, sim: &mut Sim, path: &str) {
        sim.add_dir(ROOT, path);
        self.add_parents(path);
        self.nodes.insert(path.to_owned(), Node::Dir);
    }

    fn add_file(&mut self, sim: &mut Sim, rng: &mut fastrand::Rng, path: &str, big: bool) {
        let size = random_size(rng, big);
        // Distinct per file; repeating a block keeps large files cheap.
        let block: Vec<u8> = (0..509).map(|_| rng.u8(..)).collect();
        let mut bytes = block.repeat(usize::try_from(size / 509 + 1).unwrap_or(0));
        bytes.truncate(usize::try_from(size).unwrap_or(0));
        sim.add_file(ROOT, path, &bytes);
        let fault = random_fault(rng, size);
        match fault {
            Fault::None => {}
            Fault::Shrink(at) => sim.shrink_on_send(ROOT, path, at),
            Fault::Change { after, new } => {
                let after = usize::try_from(after).unwrap_or(usize::MAX);
                sim.change_during_send(ROOT, path, after, new);
            }
        }
        let name = path.rsplit('/').next().unwrap_or_default();
        self.add_parents(path);
        self.nodes.insert(
            path.to_owned(),
            Node::File(ModelFile {
                bytes,
                content_type: content_type_of(name),
                fault,
            }),
        );
    }

    fn add_unservable(&mut self, path: &str) {
        self.add_parents(path);
        self.nodes.insert(path.to_owned(), Node::Unservable);
    }

    /// A random tree in `sim` and its model: files of random sizes and
    /// faults, directories with and without `index.html`, symlinks, a
    /// denied file, a denied directory, a special file and a dotfile.
    fn random(sim: &mut Sim, rng: &mut fastrand::Rng, big: bool) -> Tree {
        let mut tree = Tree::default();
        tree.add_dir(sim, "d");
        tree.add_dir(sim, "f");
        for path in [
            "index.html",
            "a.txt",
            "b.bin",
            "c.css",
            "d/index.html",
            "d/e.txt",
            "f/x.bin",
            ".env",
            "d/.hidden",
        ] {
            if rng.u8(0..4) != 0 {
                tree.add_file(sim, rng, path, big);
            }
        }
        if big || rng.u8(0..4) == 0 {
            tree.add_file(sim, rng, "big.bin", big);
        }
        sim.add_symlink(ROOT, "s.txt", "a.txt");
        tree.add_unservable("s.txt");
        if rng.bool() {
            sim.add_file(ROOT, "lnd/x", b"x");
            sim.add_symlink(ROOT, "ln", "lnd");
            tree.add_unservable("ln");
        }
        sim.add_file(ROOT, "secret.txt", b"secret");
        sim.deny(ROOT, "secret.txt");
        tree.add_unservable("secret.txt");
        if rng.bool() {
            sim.add_file(ROOT, "priv/x.txt", b"x");
            sim.deny(ROOT, "priv");
            tree.add_unservable("priv");
        }
        sim.add_special(ROOT, "fifo");
        tree.add_unservable("fifo");
        tree
    }

    /// How a `GET` or `HEAD` without a body of `path` (a request path on
    /// `example.test`) must be answered.
    fn resolve(&self, path: &str) -> Expect<'_> {
        let (policy, rel) = match path.strip_prefix("/g") {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => (Policy::AllowGrowth, rest),
            Some(_) | None => (Policy::Abort, path),
        };
        let slash = path.ends_with('/');
        let segments: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
        if segments.iter().any(|s| s.starts_with('.')) {
            return Expect::NotFound;
        }
        let mut key = String::new();
        let mut node = &Node::Dir;
        for segment in segments {
            match node {
                Node::Dir => {}
                Node::File(_) | Node::Unservable => return Expect::NotFound,
            }
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(segment);
            node = match self.nodes.get(&key) {
                Some(n) => n,
                None => return Expect::NotFound,
            };
        }
        match (node, slash) {
            (Node::File(f), false) => Expect::File(f, policy),
            (Node::Dir, false) => Expect::Redirect,
            (Node::Dir, true) => {
                let index = if key.is_empty() {
                    "index.html".to_owned()
                } else {
                    format!("{key}/index.html")
                };
                match self.nodes.get(&index) {
                    Some(Node::File(f)) => Expect::File(f, policy),
                    Some(Node::Dir | Node::Unservable) | None => Expect::NotFound,
                }
            }
            (Node::File(_), true) | (Node::Unservable, _) => Expect::NotFound,
        }
    }

    /// The status a request logged with `method` and `path` on
    /// `example.test` must get.
    fn expected_status(&self, method: &Method, path: &str) -> Status {
        if path.starts_with("/api/") {
            return Status::NotImplemented;
        }
        match method {
            Method::Get | Method::Head => {}
            Method::Other(_) => return Status::MethodNotAllowed,
        }
        if path.starts_with("/up/") || path.starts_with("/g/up/") {
            return Status::ContentTooLarge;
        }
        match self.resolve(path) {
            Expect::File(..) => Status::Ok,
            Expect::Redirect => Status::MovedPermanently,
            Expect::NotFound => Status::NotFound,
        }
    }
}

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
    disk_timeout: usize,
    limit_connections: usize,
    limit_per_ip: usize,
    pipelined: usize,
    aborted_truncated: usize,
    aborted_changed: usize,
    /// Aborts on the first chunk's stamp, possibly before the head was out.
    aborted_first_chunk: usize,
    /// Complete `GET` bodies of more than one chunk.
    multi_chunk: usize,
    /// Complete `GET` bodies of files that grew under `allow_growth`.
    grown: usize,
    head_ok: usize,
}

impl Coverage {
    fn check(&self) {
        let named = [
            ("Rejected(Http)", self.http),
            ("Rejected(Timeout(Header))", self.header_timeout),
            ("Rejected(Timeout(Send))", self.send_timeout),
            ("Rejected(Timeout(Disk))", self.disk_timeout),
            ("Rejected(Limit(Connections))", self.limit_connections),
            ("Rejected(Limit(ConnectionsPerIp))", self.limit_per_ip),
            ("pipelined pairs", self.pipelined),
            ("FileAborted(Truncated)", self.aborted_truncated),
            ("FileAborted(Changed)", self.aborted_changed),
            ("FileAborted on the first chunk", self.aborted_first_chunk),
            ("multi-chunk 200 bodies", self.multi_chunk),
            ("grown files sent under allow_growth", self.grown),
            ("HEAD 200", self.head_ok),
        ];
        for (status, n) in STATUSES.iter().zip(self.statuses) {
            assert!(n > 0, "no {} response in any seed", status.code());
        }
        for (what, n) in named {
            assert!(n > 0, "no {what} in any seed");
        }
    }
}

/// One complete received response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    status: Status,
    persistence: Persistence,
    body: BodyMode,
}

/// One rendering a logged response may have been sent as.
struct Rendering {
    persistence: Persistence,
    /// Length of the head (for a file response: where the body starts).
    head_len: usize,
    bytes: Vec<u8>,
}

/// A response the core logged for a connection, and how it may look.
struct Announced<'t> {
    status: Status,
    body: BodyMode,
    renderings: Vec<Rendering>,
    /// For a 200: the file and its route's policy.
    file: Option<(&'t ModelFile, Policy)>,
}

fn fixed_renderings(
    status: Status,
    persistences: &[Persistence],
    body: BodyMode,
) -> Vec<Rendering> {
    persistences
        .iter()
        .map(|&persistence| {
            let bytes = fixed(status, persistence, body);
            let mut head = Vec::new();
            render_fixed(status, persistence, BodyMode::HeadOnly, &mut head);
            Rendering {
                persistence,
                head_len: head.len(),
                bytes,
            }
        })
        .collect()
}

const BOTH: [Persistence; 2] = [Persistence::KeepAlive, Persistence::Close];

/// The response `log` announces, if it announces one, with the exact bytes
/// it may be sent as. A logged request status that does not follow from
/// the tree is an error.
fn announced<'t>(log: &Log, tree: &'t Tree) -> Result<Option<Announced<'t>>, String> {
    let (status, request) = match log {
        Log::Response {
            status, request, ..
        } => (*status, request.as_ref()),
        Log::Rejected { reason, .. } => {
            let status = match reason {
                RejectReason::Http(e) => e.status(),
                RejectReason::Timeout(TimeoutKind::Header) => Status::RequestTimeout,
                RejectReason::Timeout(TimeoutKind::Send | TimeoutKind::Disk) => return Ok(None),
                RejectReason::Limit(_) => Status::ServiceUnavailable,
                // Not emitted in this stage; a route miss is answered 404.
                RejectReason::Route(_) => Status::NotFound,
            };
            // Refusals and rejections always close, and have a body.
            return Ok(Some(Announced {
                status,
                body: BodyMode::Full,
                renderings: fixed_renderings(status, &[Persistence::Close], BodyMode::Full),
                file: None,
            }));
        }
        Log::Reset { .. } | Log::FileAborted { .. } => return Ok(None),
    };
    let body = match request.map(|r| &r.method) {
        Some(Method::Head) => BodyMode::HeadOnly,
        Some(Method::Get | Method::Other(_)) | None => BodyMode::Full,
    };
    let Some(request) = request.filter(|r| r.host.as_deref() == Some("example.test")) else {
        return Ok(Some(Announced {
            status,
            body,
            renderings: fixed_renderings(status, &BOTH, body),
            file: None,
        }));
    };
    let path = request.path.as_str();
    let expected = tree.expected_status(&request.method, path);
    if status != expected {
        return Err(format!(
            "{:?} {path} logged {} but the tree says {}",
            request.method,
            status.code(),
            expected.code()
        ));
    }
    let announced = match (status, tree.resolve(path)) {
        (Status::Ok, Expect::File(f, policy)) => {
            let renderings = BOTH
                .iter()
                .map(|&persistence| {
                    let mut bytes = Vec::new();
                    render_file_head(f.content_type, f.size(), persistence, &mut bytes);
                    let head_len = bytes.len();
                    match body {
                        BodyMode::Full => bytes.extend_from_slice(&f.bytes),
                        BodyMode::HeadOnly => {}
                    }
                    Rendering {
                        persistence,
                        head_len,
                        bytes,
                    }
                })
                .collect();
            Announced {
                status,
                body,
                renderings,
                file: Some((f, policy)),
            }
        }
        (Status::MovedPermanently, _) => {
            let location = format!("{path}/");
            let renderings = BOTH
                .iter()
                .map(|&persistence| {
                    let mut bytes = Vec::new();
                    render_redirect(&location, persistence, body, &mut bytes);
                    let mut head = Vec::new();
                    render_redirect(&location, persistence, BodyMode::HeadOnly, &mut head);
                    Rendering {
                        persistence,
                        head_len: head.len(),
                        bytes,
                    }
                })
                .collect();
            Announced {
                status,
                body,
                renderings,
                file: None,
            }
        }
        _ => Announced {
            status,
            body,
            renderings: fixed_renderings(status, &BOTH, body),
            file: None,
        },
    };
    Ok(Some(announced))
}

fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. } => *conn,
    }
}

/// Client windows; big seeds scale small windows up so megabyte files do
/// not take a million events.
fn random_window(rng: &mut fastrand::Rng, big: bool) -> Window {
    let scale = if big { 4096 } else { 1 };
    match rng.u8(0..5) {
        0 => Window::Unlimited,
        1 => Window::Bytes(rng.usize(1..64) * scale),
        2 => Window::Stalled,
        3 => Window::Budget(rng.usize(0..200)),
        _ => Window::Bytes(scale),
    }
}

/// Disk delays: mostly instant, some slow, some beyond `send_idle_timeout`.
fn random_disk_delay(rng: &mut fastrand::Rng) -> Duration {
    match rng.u8(0..8) {
        0..4 => Duration::ZERO,
        4 | 5 => Duration::from_millis(rng.u64(1..500)),
        6 => Duration::from_secs(rng.u64(1..10)),
        _ => Duration::from_secs(rng.u64(31..=45)),
    }
}

/// Send a random prefix of `client`'s queued templates.
fn send_piece(sim: &mut Sim, client: &mut Client, rng: &mut fastrand::Rng) {
    if client.queue.is_empty() {
        for _ in 0..rng.usize(1..4) {
            client.queue.push_back(random_request(rng));
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

/// A response cut short: which announced response, in which rendering,
/// and how many of its bytes arrived.
struct Cut {
    index: usize,
    head_len: usize,
    received: usize,
}

/// Check one client's received bytes against its requests, the logs and
/// the tree.
fn check_client(sim: &Sim, tree: &Tree, client: &Client, coverage: &mut Coverage) {
    let fail = |what: String| -> ! {
        panic!("conn={}: {what}\ntrace:\n{}", client.id.0, sim.trace());
    };
    let logs: Vec<&Log> = sim
        .logs()
        .iter()
        .filter(|l| log_conn(l) == client.id)
        .collect();
    let mut expected = Vec::new();
    for log in &logs {
        match announced(log, tree) {
            Ok(Some(a)) => expected.push(a),
            Ok(None) => {}
            Err(e) => fail(e),
        }
    }
    // What arrived is, in order, a prefix of what the core logged, each
    // response exactly as rendered; the last may be cut short.
    let mut seen: Vec<Seen> = Vec::new();
    let mut cut: Option<Cut> = None;
    let mut rest = client.received.as_slice();
    for (index, a) in expected.iter().enumerate() {
        if rest.is_empty() {
            break;
        }
        let offset = client.received.len() - rest.len();
        if let Some(r) = a.renderings.iter().find(|r| rest.starts_with(&r.bytes)) {
            seen.push(Seen {
                status: a.status,
                persistence: r.persistence,
                body: a.body,
            });
            rest = rest.get(r.bytes.len()..).unwrap_or_default();
            if let (Some((f, policy)), BodyMode::Full) = (a.file, a.body) {
                if f.expected_abort(policy).is_some() {
                    fail(format!(
                        "complete {} byte body of a file that must abort ({:?})",
                        f.size(),
                        f.fault
                    ));
                }
                if f.size() > CHUNK {
                    coverage.multi_chunk += 1;
                }
                if let (Policy::AllowGrowth, Fault::Change { after, new }) = (policy, f.fault) {
                    if new.size > f.size() && after < f.size().div_ceil(CHUNK) {
                        coverage.grown += 1;
                    }
                }
            }
            if a.status == Status::Ok && a.body == BodyMode::HeadOnly {
                coverage.head_ok += 1;
            }
            continue;
        }
        match a.renderings.iter().find(|r| r.bytes.starts_with(rest)) {
            Some(r) => {
                cut = Some(Cut {
                    index,
                    head_len: r.head_len,
                    received: rest.len(),
                });
                rest = &[];
                break;
            }
            None => fail(format!(
                "unrecognised response bytes at offset {offset} (expected a {})",
                a.status.code()
            )),
        }
    }
    if !rest.is_empty() {
        let statuses: Vec<Status> = seen.iter().map(|s| s.status).collect();
        fail(format!(
            "{} bytes after the logged responses {statuses:?}",
            rest.len()
        ));
    }
    let statuses: Vec<Status> = seen.iter().map(|s| s.status).collect();
    let announced_statuses: Vec<Status> = expected.iter().map(|a| a.status).collect();
    let truncated = cut.is_some();
    // Less than announced, or cut short, only when the write was abandoned
    // (send or disk timeout, reset, file abort) or a refusal was closed at
    // once.
    if truncated || statuses.len() < announced_statuses.len() {
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
                "received {statuses:?} (truncated: {truncated}) of {announced_statuses:?}"
            ));
        }
    }
    // A file abort is the connection's last word, right after the 200 of a
    // GET whose file must abort that way, and the body is cut exactly where
    // the fault strikes.
    if let Some(at) = logs
        .iter()
        .position(|l| matches!(l, Log::FileAborted { .. }))
    {
        let Some(Log::FileAborted { error, .. }) = logs.get(at) else {
            fail("unreachable".to_owned());
        };
        if at + 1 != logs.len() {
            fail(format!("logs after FileAborted: {logs:?}"));
        }
        let last = expected.len().checked_sub(1);
        let Some(a) = last.and_then(|i| expected.get(i)) else {
            fail("FileAborted without a 200".to_owned());
        };
        let Some((f, policy)) = a.file.filter(|_| a.body == BodyMode::Full) else {
            fail(format!(
                "FileAborted after a {} {:?}",
                a.status.code(),
                a.body
            ));
        };
        let Some((want, body_at)) = f.expected_abort(policy) else {
            fail(format!(
                "FileAborted({error:?}) for a file with {:?}",
                f.fault
            ));
        };
        if *error != want {
            fail(format!(
                "FileAborted({error:?}), expected {want:?} for {:?} under {policy:?}",
                f.fault
            ));
        }
        match (body_at, &cut) {
            (Some(n), Some(c))
                if Some(c.index) == last
                    && u64::try_from(c.received).ok() == Some(c.head_len as u64 + n) => {}
            (None, Some(c)) if Some(c.index) == last && c.received <= c.head_len => {}
            (None, None) if Some(statuses.len()) == last => {}
            _ => fail(format!(
                "FileAborted({error:?}) for {:?} of a {} byte file, but received {:?} bytes \
                 (head {:?}) of response {:?}",
                f.fault,
                f.size(),
                cut.as_ref().map(|c| c.received),
                cut.as_ref().map(|c| c.head_len),
                cut.as_ref().map(|c| c.index),
            )),
        }
        match error {
            FileError::Truncated => coverage.aborted_truncated += 1,
            FileError::Changed => coverage.aborted_changed += 1,
            FileError::NotFound
            | FileError::Symlink
            | FileError::PermissionDenied
            | FileError::Other => {}
        }
        if body_at.is_none() {
            coverage.aborted_first_chunk += 1;
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
    // One seed in three serves files up to 1 MiB.
    let big = rng.u8(0..3) == 0;
    let tree = Tree::random(&mut sim, &mut rng, big);
    sim.set_disk_delay(random_disk_delay(&mut rng));
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
                sim.set_client_window(id, random_window(&mut rng, big));
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
                60..68 => sim.set_client_window(client.id, random_window(&mut rng, big)),
                68..72 => sim.reset(client.id),
                72..78 => {
                    sim.send_eof(client.id);
                    client.eof = true;
                }
                78..86 => sim.advance(Duration::from_millis(rng.u64(0..12_000))),
                86..90 => sim.advance(Duration::from_secs(rng.u64(20..=90))),
                90..95 => sim.set_disk_delay(random_disk_delay(&mut rng)),
                _ => sim.set_read_chunk(client.id, rng.usize(1..200)),
            }
        }
        if rng.u8(0..4) != 0 {
            sim.run_until_idle();
        }
        collect(&mut sim, &mut clients);
    }
    // Wind down: every client sends EOF, then 5 minutes pass (longer than
    // any disk delay, so every open and readahead is answered).
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
    assert_eq!(
        sim.open_files(),
        0,
        "files left open\ntrace:\n{}",
        sim.trace()
    );
    for client in &clients {
        check_client(&sim, &tree, client, coverage);
    }
    for log in sim.logs() {
        if let Log::Rejected { reason, .. } = log {
            match reason {
                RejectReason::Http(_) => coverage.http += 1,
                RejectReason::Timeout(TimeoutKind::Header) => coverage.header_timeout += 1,
                RejectReason::Timeout(TimeoutKind::Send) => coverage.send_timeout += 1,
                RejectReason::Timeout(TimeoutKind::Disk) => coverage.disk_timeout += 1,
                RejectReason::Limit(LimitKind::Connections) => coverage.limit_connections += 1,
                RejectReason::Limit(LimitKind::ConnectionsPerIp) => coverage.limit_per_ip += 1,
                RejectReason::Route(_) => {}
            }
        }
    }
    // Every connection slot is free again, orphaned opens included: a full
    // house of new connections is accepted.
    let logged = sim.logs().len();
    for i in 0..6 {
        sim.connect(IpAddr::V4(Ipv4Addr::new(198, 51, 100, i)));
    }
    assert_eq!(
        sim.logs().get(logged..).unwrap_or_default(),
        [],
        "connection slots still taken after the run\ntrace:\n{}",
        sim.trace()
    );
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
                "COVERAGE statuses={:?} http={} header={} send={} disk={} conns={} per_ip={} \
                 pipelined={} truncated={} changed={} first_chunk={} multi_chunk={} grown={} \
                 head_ok={}",
                coverage.statuses,
                coverage.http,
                coverage.header_timeout,
                coverage.send_timeout,
                coverage.disk_timeout,
                coverage.limit_connections,
                coverage.limit_per_ip,
                coverage.pipelined,
                coverage.aborted_truncated,
                coverage.aborted_changed,
                coverage.aborted_first_chunk,
                coverage.multi_chunk,
                coverage.grown,
                coverage.head_ok
            );
            coverage.check();
        }
    }
}
