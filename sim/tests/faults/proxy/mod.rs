//! The randomized proxy net: seeded runs of random clients against random
//! backends on proxy routes (with and without `strip_prefix`, varied body
//! limits and timeouts), smuggling attempts and hostile backends among
//! them. Every exchange is predicted by a model of its own (`request.rs`,
//! `response.rs`, `outcome.rs`; drawn by `draw.rs`), written from the
//! spec: the backend must receive exactly the rewritten request and the
//! client's body, the client exactly the rewritten head and the backend's
//! body bytes, a cut connection with its log, or the predicted 4xx/5xx;
//! nothing after a framing end is ever forwarded (NFR-SEC-02, NFR-SEC-03).
//! Backend connections are pooled on the routes a seed draws pooled, and
//! every reuse and retry is judged by the spec's rules (`pool.rs`). Routes
//! drawn with `internal_files` get Rack's hints; file headers are forged
//! by clients and sent by backends, and never pass (`files.rs`); their
//! backends' `200`s name files of a random tree below the route's root,
//! which the client gets as the spec says (`internal.rs`,
//! `file_model.rs`).
//!
//! A seed is a series of rounds: several stories (one client connection
//! each, a series of exchanges on it) interleaved at random without time
//! passing, or a story with a timed exchange (connect, response, head,
//! body and send timeouts) that waits until the other stories of its round
//! are done, so the clock only runs out deadlines the model knows. Every
//! round ends with all its connections and upstreams closed; the seed ends
//! with no open upstream, file or connection and every slot free. One seed
//! in five shuts the server down in the middle of a round and ends with
//! that round (`drain.rs`).

mod check;
mod chunks;
mod drain;
mod draw;
mod drive;
mod file_model;
mod files;
mod internal;
mod outcome;
mod pool;
mod request;
mod response;
mod round_end;
mod timed;

use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::time::Duration;

use durchlauferhitzer_core::http::chunked::ChunkedError;
use durchlauferhitzer_core::http::response_head::ResponseError;
use durchlauferhitzer_core::{BackendFailure, BackendId, UpstreamHolder, UpstreamId};

use super::*;
use check::*;
use chunks::*;
use drain::*;
use draw::*;
use drive::*;
use file_model::*;
use files::*;
use internal::*;
use outcome::*;
use pool::*;
use request::*;
use response::*;
use round_end::*;
use timed::*;

/// Seeds the proxy net runs (debug build: about 15 s; 1000 so the thin
/// pool, retry and drain gates each see more than a handful).
pub(crate) const PROXY_SEEDS: u64 = 1000;

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
    /// Backend connections are pooled (FR-PROXY-02); else `pool off;`.
    pub(crate) pool: bool,
    /// The route's `internal_files` (FR-CFG-26), if any.
    pub(crate) files: Option<RouteFiles>,
    /// Its `on_file_change` (FR-STATIC-14; set only with `files`).
    pub(crate) policy: Policy,
    /// The file tree below its root (shared by routes with one root).
    pub(crate) tree: Option<Rc<FileTree>>,
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
    /// `backend_idle_timeout` and `backend_idle_max` (FR-PROXY-02).
    pub(crate) idle: Duration,
    pub(crate) idle_max: u8,
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

    /// An upstream's receive buffer, by NFR-RES-01: `max(32 KiB,
    /// max_head)`. Only bytes within it can arrive together with a
    /// response head.
    pub(crate) fn upstream_recv_capacity(&self) -> usize {
        self.max_head.max(32 * 1024)
    }

    /// A sim of this config, its clock set (`dated_sim`), the routes'
    /// file trees made.
    pub(crate) fn sim(&self) -> Sim {
        let mut sim = dated_sim(&self.text);
        let mut done: Vec<&'static str> = Vec::new();
        for route in &self.routes {
            if let (Some(files), Some(tree)) = (route.files, &route.tree) {
                if !done.contains(&files.root()) {
                    tree.install(&mut sim);
                    done.push(files.root());
                }
            }
        }
        sim
    }

    pub(crate) fn random(rng: &mut fastrand::Rng) -> NetConfig {
        let mut routes = [
            RouteConfig {
                prefix: "/api",
                strip: true,
                addr: "127.0.0.1:3000",
                backend: BackendId(0),
                max_request_body: body_limit(rng),
                max_response_body: body_limit(rng),
                pool: rng.u8(0..4) != 0,
                files: RouteFiles::random(rng),
                policy: Policy::Abort,
                tree: None,
            },
            RouteConfig {
                prefix: "/raw",
                strip: false,
                addr: "[::1]:3001",
                backend: BackendId(1),
                max_request_body: body_limit(rng),
                max_response_body: body_limit(rng),
                pool: rng.u8(0..4) != 0,
                files: RouteFiles::random(rng),
                policy: Policy::Abort,
                tree: None,
            },
        ];
        // A tree per root, from a stream of its own; `allow_growth` on a
        // file route now and then.
        let mut trees = fastrand::Rng::with_seed(rng.u64(..));
        let [a, b] = route_trees(&mut trees, [routes[0].files, routes[1].files]);
        for (route, tree) in routes.iter_mut().zip([a, b]) {
            route.tree = tree;
            if route.files.is_some() && trees.u8(0..3) == 0 {
                route.policy = Policy::AllowGrowth;
            }
        }
        let max_head = *rng.choice(&[2048, 4096, 16 * 1024]).unwrap_or(&4096);
        let secs = |rng: &mut fastrand::Rng, lo, hi| Duration::from_secs(rng.u64(lo..=hi));
        let connect = secs(rng, 1, 3);
        let response = secs(rng, 2, 8);
        let body_idle = secs(rng, 2, 8);
        let send_idle = secs(rng, 3, 10);
        let idle = secs(rng, 1, 4);
        let idle_max = if rng.u8(0..3) == 0 { 2 } else { 1 };
        let max_connections = 8;
        let max_connections_per_ip = 4;
        let route = |r: &RouteConfig| {
            format!(
                "  path {} {{ proxy {};{}{} max_request_body {}; max_response_body {};{}{} }}\n",
                r.prefix,
                r.addr,
                if r.pool { "" } else { " pool off;" },
                if r.strip { " strip_prefix;" } else { "" },
                limit_text(r.max_request_body),
                limit_text(r.max_response_body),
                RouteFiles::setting(r.files),
                match r.policy {
                    Policy::Abort => "",
                    Policy::AllowGrowth => " on_file_change allow_growth;",
                },
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
  backend_idle_timeout {}s;
  backend_idle_max {idle_max};
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
            idle.as_secs(),
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
            idle,
            idle_max,
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
    "client end of stream mid-body",
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
    "400 body on GET or HEAD",
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
    // Bytes, then a reset behind them (`Sim::backend_reset_after`).
    "reset with bytes: none",
    "reset with bytes: within the head",
    "reset with bytes: within the body",
    "reset with bytes: after the head, no cut",
    "cut Reset with every byte before it",
    "client send timeout",
    "client reset",
    "client end of stream",
    "interim heads",
    "request head at the limit",
    "request head just too large",
    "backend head at the limit",
    "backend head just too large",
    "large body",
    // Shutdown (FR-PROC-05, `drain.rs`).
    "drain: 503 to a request",
    "drain: closing head",
    "drain: closed before a request",
    "abort: exchange cut",
    "abort: response cut short",
    // Pooling and the safe retry (FR-PROXY-02, -26, `pool.rs`).
    "pool: reuse",
    "pool: reuse by another client",
    "pool: idle close by byte",
    "pool: idle close by eof",
    "pool: idle close by error",
    "pool: idle close by timeout",
    "pool: idle close for room",
    "pool: idle close by a drain",
    "pool: not silent at reuse",
    "retry: replayed, relayed",
    "retry: replayed, failed again",
    "retry: refused, 502",
    "retry: none after a response byte",
    "retry: none in a drain",
    // Restart (FR-PROC-07, `drain.rs`).
    "restart: plain drain",
    "restart: drain cut by the abort",
    "restart: served with close",
    // File headers (FR-PROXY-32, `files.rs`).
    "forged-file-header",
    "underscore-spelled-header",
    "file-header-on-non-200",
    "other-file-header-on-200",
    "x-accel-star-stripped",
];

/// Branches the nets allow but no seed is known to reach: counted and
/// printed (`PROXY RARE <what>=<count>`, zero included) by every run, not
/// gated. "restart: closed before a request" (`silent_stop` in
/// `drain.rs`) needs a keep-alive deadline to pass during a restart's
/// drain before the client's next request is read; the drain falls only
/// in rounds without a timed scenario, so none was seen in `0..200000`.
pub(crate) const RARE: &[&str] = &["restart: closed before a request"];

impl ProxyCoverage {
    /// The `PROXY RARE` lines, one per `RARE` branch.
    pub(crate) fn rare(&self) -> Vec<String> {
        RARE.iter()
            .map(|what| {
                format!(
                    "PROXY RARE {what}={}",
                    self.seen.get(what).copied().unwrap_or(0)
                )
            })
            .collect()
    }

    /// Every `REQUIRED` name reached; each file name (`file_model.rs`) as
    /// often as `seeds` seeds call for.
    pub(crate) fn check(&self, seeds: u64) {
        check_file_coverage(&self.seen, seeds);
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
        Some(seed) => {
            let plan = Rc::new(Cell::new(shutdown_plan(seed, PROXY_SPAN)));
            run_reporting(seed, &plan, || {
                run_proxy_seed(seed, &mut coverage, Rc::clone(&plan));
            });
        }
        None => {
            let range = seed_range(0..PROXY_SEEDS);
            let seeds = range.end - range.start;
            for seed in range {
                let plan = Rc::new(Cell::new(shutdown_plan(seed, PROXY_SPAN)));
                run_reporting(seed, &plan, || {
                    run_proxy_seed(seed, &mut coverage, Rc::clone(&plan));
                });
            }
            eprintln!("PROXY COVERAGE {:?}", coverage.seen);
            for line in coverage.rare() {
                eprintln!("{line}");
            }
            coverage.check(seeds);
        }
    }
}

#[test]
fn rare_branches_are_reported_even_at_zero() {
    let mut coverage = ProxyCoverage::default();
    assert_eq!(
        coverage.rare(),
        ["PROXY RARE restart: closed before a request=0"]
    );
    coverage.add("restart: closed before a request");
    assert_eq!(
        coverage.rare(),
        ["PROXY RARE restart: closed before a request=1"]
    );
}
