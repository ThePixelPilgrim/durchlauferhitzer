//! The randomized proxy net: seeded runs of random clients against random
//! backends on proxy routes (with and without `strip_prefix`, varied body
//! limits and timeouts), smuggling attempts and hostile backends among
//! them. Every exchange is predicted by a model of its own (`request.rs`,
//! `response.rs`, `outcome.rs`), written from the spec: the backend must
//! receive exactly the rewritten request and the client's body, the client
//! exactly the rewritten head and the backend's body bytes, a cut
//! connection with its log, or the predicted 4xx/5xx; nothing after a
//! framing end is ever forwarded (NFR-SEC-02, NFR-SEC-03).
//!
//! A seed is a series of rounds: several stories (one client connection
//! each, a series of exchanges on it) interleaved at random without time
//! passing, or a story with a timed exchange (connect, response, head,
//! body and send timeouts) that waits until the other stories of its round
//! are done, so the clock only runs out deadlines the model knows. Every
//! round ends with all its connections and upstreams closed; the seed ends
//! with no open upstream, file or connection and every slot free.

mod check;
mod chunks;
mod drive;
mod outcome;
mod request;
mod response;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Duration;

use durchlauferhitzer_core::http::chunked::ChunkedError;
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::{BackendFailure, BackendId, UpstreamId};

use super::*;
use check::*;
use chunks::*;
use drive::*;
use outcome::*;
use request::*;
use response::*;

/// Seeds the proxy net runs (debug build: a few seconds).
pub(crate) const PROXY_SEEDS: u64 = 400;

/// `max_request_line` of every generated config.
pub(crate) const MAX_REQUEST_LINE: usize = 1024;

/// A proxy route of the generated config.
#[derive(Debug)]
pub(crate) struct RouteConfig {
    pub(crate) prefix: &'static str,
    pub(crate) strip: bool,
    pub(crate) addr: &'static str,
    pub(crate) backend: BackendId,
    pub(crate) max_request_body: Option<u64>,
    pub(crate) max_response_body: Option<u64>,
}

/// A random config: two proxy routes next to a static one.
#[derive(Debug)]
pub(crate) struct NetConfig {
    pub(crate) text: String,
    pub(crate) routes: [RouteConfig; 2],
    pub(crate) max_head: usize,
    pub(crate) max_connections: u8,
    /// Below `max_connections`, so the per-IP slots are checked too.
    pub(crate) max_connections_per_ip: u8,
    pub(crate) connect: Duration,
    pub(crate) response: Duration,
    pub(crate) body_idle: Duration,
    pub(crate) send_idle: Duration,
}

fn body_limit(rng: &mut fastrand::Rng) -> Option<u64> {
    match rng.u8(0..6) {
        0 => Some(rng.u64(0..64)),
        1 => Some(rng.u64(64..5000)),
        2 => Some(64 * 1024),
        3 => Some(1024 * 1024),
        _ => None,
    }
}

fn limit_text(limit: Option<u64>) -> String {
    limit.map_or_else(|| "unlimited".to_owned(), |n| n.to_string())
}

impl NetConfig {
    /// A client connection's receive buffer, by NFR-RES-01:
    /// `max(max_head, max_request_line + 2)`. Only bytes within it can
    /// arrive together with the request head.
    pub(crate) fn client_recv_capacity(&self) -> usize {
        self.max_head.max(MAX_REQUEST_LINE + 2)
    }

    /// An upstream's receive buffer, by NFR-RES-01: `max(16 KiB,
    /// max_head)`. Only bytes within it can arrive together with a
    /// response head.
    pub(crate) fn upstream_recv_capacity(&self) -> usize {
        self.max_head.max(16 * 1024)
    }

    pub(crate) fn random(rng: &mut fastrand::Rng) -> NetConfig {
        let routes = [
            RouteConfig {
                prefix: "/api",
                strip: true,
                addr: "127.0.0.1:3000",
                backend: BackendId(0),
                max_request_body: body_limit(rng),
                max_response_body: body_limit(rng),
            },
            RouteConfig {
                prefix: "/raw",
                strip: false,
                addr: "[::1]:3001",
                backend: BackendId(1),
                max_request_body: body_limit(rng),
                max_response_body: body_limit(rng),
            },
        ];
        let max_head = *rng.choice(&[2048, 4096, 16 * 1024]).unwrap_or(&4096);
        let secs = |rng: &mut fastrand::Rng, lo, hi| Duration::from_secs(rng.u64(lo..=hi));
        let connect = secs(rng, 1, 3);
        let response = secs(rng, 2, 8);
        let body_idle = secs(rng, 2, 8);
        let send_idle = secs(rng, 3, 10);
        let max_connections = 8;
        let max_connections_per_ip = 4;
        let route = |r: &RouteConfig| {
            format!(
                "  path {} {{ proxy {};{} max_request_body {}; max_response_body {}; }}\n",
                r.prefix,
                r.addr,
                if r.strip { " strip_prefix;" } else { "" },
                limit_text(r.max_request_body),
                limit_text(r.max_response_body)
            )
        };
        let text = format!(
            "listen 127.0.0.1:8080;
limits {{
  max_request_line {MAX_REQUEST_LINE};
  max_head {max_head};
  header_timeout 20s;
  keepalive_timeout 120s;
  send_idle_timeout {}s;
  backend_connect_timeout {}s;
  backend_response_timeout {}s;
  body_idle_timeout {}s;
  max_connections {max_connections};
  max_connections_per_ip {max_connections_per_ip};
}}
host example.test {{
  path /files {{ static /srv/www; }}
{}{}}}",
            send_idle.as_secs(),
            connect.as_secs(),
            response.as_secs(),
            body_idle.as_secs(),
            route(&routes[0]),
            route(&routes[1]),
        );
        NetConfig {
            text,
            routes,
            max_head,
            max_connections,
            max_connections_per_ip,
            connect,
            response,
            body_idle,
            send_idle,
        }
    }
}

/// What the proxy net reached, by name, summed over all seeds.
#[derive(Default)]
pub(crate) struct ProxyCoverage {
    pub(crate) seen: BTreeMap<&'static str, usize>,
}

impl ProxyCoverage {
    pub(crate) fn add(&mut self, what: &'static str) {
        *self.seen.entry(what).or_default() += 1;
    }
}

/// Every bucket that must be above zero across the seeds.
pub(crate) const REQUIRED: &[&str] = &[
    "status 400",
    "status 404",
    "status 408",
    "413 Content-Length over the proxy limit",
    "413 static route with a body",
    "status 417",
    "status 431",
    "status 502",
    "status 504",
    "100 Continue",
    "body None",
    "body Length",
    "body Chunked",
    "body UntilClose",
    "cut after head",
    "early response",
    "eager response",
    "head deadline during the upload",
    "pipelined after a chunked body",
    "400 bad request chunk",
    "400 Connection names framing",
    "400 duplicate X-Forwarded-For",
    // NFR-SEC-02.
    "CL+TE",
    "duplicate CL",
    "chunk extension",
    "chunk trailer",
    "request hidden in a body",
    // Each branch of an outcome that depends on the split.
    "body error before connect",
    "request body over the limit, silent",
    "body error after connect",
    "body error, then refused",
    "refused, body read with the head",
    "refused, body unread",
    "connect timeout, body read with the head",
    "connect timeout, body unread",
    "backend body stall, body all read",
    "backend body stall, body unread",
    "eager, body all read",
    "eager, body unread",
    "body error with the response head",
    "body error after the response head",
    "bad CL",
    "CL list",
    "CL overflow",
    "TE twice",
    "unsupported TE",
    "obs-fold",
    "whitespace before a colon",
    "bare LF in the head",
    "NUL in a value",
    "obs-text value",
    "control byte in value",
    "duplicate Host",
    "bad Expect",
    "bad chunk size",
    "chunk size overflow",
    "chunk size over 32 characters",
    "empty chunk size",
    "bad chunk line end",
    "request body over the limit",
    // NFR-SEC-03.
    "101 from a backend",
    "too many interim heads",
    "TE in an HTTP/1.0 response",
    "backend Connection names framing",
    "malformed backend head",
    "bad backend framing",
    "backend head too large",
    "duplicate backend TE",
    "control byte from a backend",
    "backend CL overflow",
    "defect in an interim head",
    "502 BadChunk",
    "bytes after a backend response",
    "CR in a backend head",
    "LF in a backend head",
    "obs-text from a backend",
    // Failures and timeouts.
    "502 Refused",
    "502 Reset",
    "502 ClosedEarly",
    "502 BadResponse",
    "502 BodyTooLarge",
    "502 TransferEncodingToHttp10",
    "504 ConnectTimeout",
    "504 ResponseTimeout",
    "504 BodyTimeout",
    "cut Reset",
    "cut ClosedEarly",
    "cut BadChunk",
    "cut BodyTooLarge",
    "cut ResponseTimeout",
    "client send timeout",
    "client reset",
    "client end of stream",
    "interim heads",
    "request head at the limit",
    "request head just too large",
    "backend head at the limit",
    "backend head just too large",
    "large body",
];

impl ProxyCoverage {
    pub(crate) fn check(&self) {
        for what in REQUIRED {
            assert!(
                self.seen.get(what).copied().unwrap_or(0) > 0,
                "no {what} in any proxy seed"
            );
        }
    }
}

#[test]
fn random_proxy_schedules() {
    let mut coverage = ProxyCoverage::default();
    match replay_seed() {
        Some(seed) => run_reporting(seed, || run_proxy_seed(seed, &mut coverage)),
        None => {
            for seed in 0..PROXY_SEEDS {
                run_reporting(seed, || run_proxy_seed(seed, &mut coverage));
            }
            eprintln!("PROXY COVERAGE {:?}", coverage.seen);
            coverage.check();
        }
    }
}
