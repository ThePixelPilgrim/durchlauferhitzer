//! Random requests and schedule steps the randomized runs draw from.

use super::*;

// ---------------------------------------------------------------------------
// Randomized fault schedules

pub(crate) const SEEDS: u64 = 1500;

/// Every status the core can send to these clients; each must be seen
/// across the seeds. (These backends refuse every connect.)
pub(crate) const STATUSES: [Status; 12] = [
    Status::Ok,
    Status::MovedPermanently,
    Status::BadRequest,
    Status::NotFound,
    Status::MethodNotAllowed,
    Status::RequestTimeout,
    Status::ContentTooLarge,
    Status::UriTooLong,
    Status::RequestHeaderFieldsTooLarge,
    Status::BadGateway,
    Status::ServiceUnavailable,
    Status::HttpVersionNotSupported,
];

/// Request templates without static files that the random clients draw
/// from.
pub(crate) const REQUESTS: &[&[u8]] = &[
    // 404, keep-alive.
    b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n",
    b"GET /y HTTP/1.1\r\nHost: other.test\r\n\r\n",
    b"HEAD / HTTP/1.1\r\nHost: other.test\r\n\r\n",
    // Matched proxy route, whose backend refuses every connect: 502,
    // keep-alive (with a request body: close unless all of it was read).
    b"GET /api/v HTTP/1.1\r\nHost: example.test\r\n\r\n",
    b"HEAD /api/v HTTP/1.1\r\nHost: example.test\r\n\r\n",
    b"POST /api/v HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi",
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
pub(crate) const STATIC_PATHS: &[&str] = &[
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
    "grow.txt",
    // Escapes the path may keep: decoded once by static serving.
    "sp%20ace.txt",
    "%C3%A4.txt",
    // Escapes of unreserved characters (and an encoded dot): 400.
    "%61.txt",
    "d/%7Ee.txt",
    "%2D",
    "d/e%2Etxt",
];

/// Queries a static request may carry (kept in a redirect's `Location`).
pub(crate) const QUERIES: &[&str] = &["", "", "", "?", "?x=1", "?q=%61b%2F;c"];

/// Lengths of the long queries of long seeds, around the point where a
/// directory redirect stops fitting the send buffer.
pub(crate) const LONG_QUERY_LENS: &[usize] = &[100, 300, 500, 700, 900];

/// Every query the generator can produce.
pub(crate) fn all_queries() -> Vec<String> {
    let mut all: Vec<String> = QUERIES.iter().map(|q| (*q).to_owned()).collect();
    all.extend(
        LONG_QUERY_LENS
            .iter()
            .map(|&n| format!("?{}", "q".repeat(n))),
    );
    all
}

/// The static root both static routes serve.
pub(crate) const ROOT: &str = "/srv/www";
/// How much one `Readahead` covers (the core's chunk size).
pub(crate) const CHUNK: u64 = 256 * 1024;
pub(crate) const MIB: u64 = 1024 * 1024;

/// A static request: a path under `/` (abort on change), `/g` (allow
/// growth) or `/.well-known` (abort), maybe with a query. Bodies only go to
/// `up/` paths, so the expected status of every logged request follows
/// from its method and path alone (and, for a redirect, the query).
pub(crate) fn random_static_request(rng: &mut fastrand::Rng, long_dir: Option<&str>) -> Vec<u8> {
    let host = "Host: example.test\r\n";
    // Long seeds: the long directory, with or without its slash, and a
    // query of any length.
    if let Some(dir) = long_dir.filter(|_| rng.u8(0..3) == 0) {
        let slash = if rng.u8(0..3) == 0 { "/" } else { "" };
        // Mostly long queries, around the point where the redirect stops
        // fitting the send buffer.
        let query = match rng.u8(0..4) {
            0 => rng.choice(QUERIES).copied().unwrap_or_default().to_owned(),
            _ => format!(
                "?{}",
                "q".repeat(rng.choice(LONG_QUERY_LENS).copied().unwrap_or(0))
            ),
        };
        let method = if rng.bool() { "GET" } else { "HEAD" };
        return format!("{method} /{dir}{slash}{query} HTTP/1.1\r\n{host}\r\n").into_bytes();
    }
    let prefix = match rng.u8(0..6) {
        0 | 1 => "/".to_owned(),
        2 | 3 => "/g/".to_owned(),
        4 => "/.well-known/".to_owned(),
        // The route `/@%C3%A4`, spelled any way that decodes the same.
        _ => format!(
            "/{}/",
            rng.choice(ESCAPED_ROUTE).copied().unwrap_or_default()
        ),
    };
    let prefix = prefix.as_str();
    // Bias `/g/` toward the growing file, so growth sent under
    // `allow_growth` is common.
    let rel = match prefix {
        "/g/" if rng.u8(0..3) == 0 => "grow.txt",
        _ => rng.choice(STATIC_PATHS).copied().unwrap_or_default(),
    };
    let path = match (rel, prefix) {
        ("", "/") => prefix.to_owned(),
        ("", _) if rng.bool() => prefix.trim_end_matches('/').to_owned(),
        _ => format!("{prefix}{rel}"),
    };
    let query = rng.choice(QUERIES).copied().unwrap_or_default();
    let path = format!("{path}{query}");
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

pub(crate) fn random_request(rng: &mut fastrand::Rng, long_dir: Option<&str>) -> Vec<u8> {
    if rng.u8(0..10) < 7 {
        random_static_request(rng, long_dir)
    } else {
        rng.choice(REQUESTS).map(|r| r.to_vec()).unwrap_or_default()
    }
}

/// Client windows; big seeds scale small windows up so megabyte files do
/// not take a million events.
pub(crate) fn random_window(rng: &mut fastrand::Rng, big: bool) -> Window {
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
pub(crate) fn random_disk_delay(rng: &mut fastrand::Rng) -> Duration {
    match rng.u8(0..8) {
        0..4 => Duration::ZERO,
        4 | 5 => Duration::from_millis(rng.u64(1..500)),
        6 => Duration::from_secs(rng.u64(1..10)),
        _ => Duration::from_secs(rng.u64(31..=45)),
    }
}

/// Send a random prefix of `client`'s queued templates.
pub(crate) fn send_piece(
    sim: &mut Sim,
    client: &mut Client,
    rng: &mut fastrand::Rng,
    long_dir: Option<&str>,
) {
    if client.queue.is_empty() {
        for _ in 0..rng.usize(1..4) {
            client.queue.push_back(random_request(rng, long_dir));
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
pub(crate) fn collect(sim: &mut Sim, clients: &mut [Client]) {
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
