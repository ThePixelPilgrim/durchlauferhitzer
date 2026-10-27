//! Simulation driver for the durchlauferhitzer core (test-only).
//!
//! `Sim` stands in for the network and the clock. It follows the same driver
//! contract as the production epoll driver (see `durchlauferhitzer_core::Core`):
//! it reads client bytes directly into `Core::recv_buffer`, writes directly
//! out of `Core::send_buffer`, performs actions in order and never reuses a
//! `ConnId`. Every event and action is recorded in [`Sim::trace`], and any
//! violation of the contract by the core panics with the trace attached.
//!
//! Client-side calls (`connect`, `reset`) deliver their event at once;
//! `send` and `send_eof` only queue data. [`Sim::run_until_idle`] pumps the
//! queues through the core until nothing moves.
//!
//! Files live in an in-memory file system (see `fs`), one tree per static
//! root, named by the root's path as written in the config. Disk operations
//! (`OpenFile`, `Readahead`) complete after [`Sim::set_disk_delay`] of
//! virtual time: with no delay inside `run_until_idle`, otherwise inside
//! [`Sim::advance`], merged with the core's deadlines. `sendfile` copies
//! straight from the file system into the client's received bytes.
//!
//! Backends are scripted per upstream (see `backend`): a connect is
//! accepted, refused or left hanging by backend address; the test then
//! reads what the backend received and makes it send bytes, close or reset.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod backend;
mod disk;
mod fs;

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::http::chunked::MAX_UNCONSUMED;
use durchlauferhitzer_core::{
    Action, BodyPending, ConnError, ConnId, Core, Event, FileId, Instant, Interest, Log, UpstreamId,
};

use crate::backend::Backends;
pub use crate::backend::ConnectBehaviour;
use crate::disk::{DiskOp, FileTrack};
use crate::fs::Fs;

/// Default bound on pump steps in one `run_until_idle` before the sim
/// declares a livelock.
const STEP_LIMIT: usize = 1_000_000;

/// How many bytes the simulated client (or backend) accepts per `Sent`
/// (`UpstreamSent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Unlimited,
    Bytes(usize),
    Stalled,
    /// Accept at most this many more bytes in total, then stall (the budget
    /// shrinks with every `Sent`).
    Budget(usize),
}

impl Window {
    /// How many bytes one write may hand over now.
    fn size(self) -> usize {
        match self {
            Window::Unlimited => usize::MAX,
            Window::Bytes(n) | Window::Budget(n) => n,
            Window::Stalled => 0,
        }
    }

    /// The window took `n` bytes (a budget shrinks).
    fn take(&mut self, n: usize) {
        if let Window::Budget(left) = self {
            *left -= n;
        }
    }
}

/// Connection state as the simulated network sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimConnState {
    Open,
    WriteShutdown,
    Closed,
}

/// Whether the client still talks to the server on this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientSide {
    Live,
    /// The client reset the connection; no more I/O events are delivered.
    Reset,
}

/// Progress of the client's end-of-stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eof {
    NotSent,
    Queued,
    Delivered,
}

#[derive(Debug)]
struct SimConn {
    to_server: VecDeque<u8>,
    eof: Eof,
    client: ClientSide,
    received: Vec<u8>,
    read_chunk: usize,
    window: Window,
    interest: Interest,
    state: SimConnState,
    /// Send capacity the core reported right after `Accepted` (0 until
    /// recorded). It stays the same while the connection is open, except
    /// once it has proxied: then it may also be the head buffer's or the
    /// upstream receive buffer's capacity while a response is relayed.
    send_capacity: usize,
}

/// A read into a core buffer not yet reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Conn(ConnId),
    Upstream(UpstreamId),
}

/// The simulated network, clock and client population.
#[derive(Debug)]
pub struct Sim {
    core: Core,
    now: Instant,
    next_id: u64,
    conns: BTreeMap<ConnId, SimConn>,
    logs: Vec<Log>,
    trace: String,
    actions: Vec<Action>,
    /// Set while bytes written into a connection's `recv_buffer` (or an
    /// upstream's) have not yet been reported with `Received`
    /// (`UpstreamReceived`).
    pending_read: Option<Pending>,
    step_limit: usize,
    fs: Fs,
    disk_delay: Duration,
    /// Disk completions in order of their due time.
    disk: VecDeque<(Instant, DiskOp)>,
    files: BTreeMap<FileId, FileTrack>,
    /// The connection the event being handled belongs to, if any.
    context: Option<ConnId>,
    backends: Backends,
}

impl Sim {
    /// A simulation of a server loaded from `config`. Panics on an invalid
    /// config.
    pub fn new(config: &str) -> Sim {
        let config = match load(config) {
            Ok(c) => c,
            Err(e) => panic!("invalid sim config: {e:?}"),
        };
        let limits = config.limits.clone();
        Sim {
            core: Core::new(config),
            now: Instant::from_nanos(0),
            next_id: 1,
            conns: BTreeMap::new(),
            logs: Vec::new(),
            trace: String::new(),
            actions: Vec::new(),
            pending_read: None,
            step_limit: STEP_LIMIT,
            fs: Fs::default(),
            disk_delay: Duration::ZERO,
            disk: VecDeque::new(),
            files: BTreeMap::new(),
            context: None,
            backends: Backends::new(&limits),
        }
    }

    /// Tell the core the wall-clock time (`Core::set_date`): every
    /// response it renders from now on carries it as `Date`. The sim's
    /// clock is virtual and never sets it by itself, so until a test calls
    /// this no `Date` is sent.
    pub fn set_date(&mut self, unix_secs: u64) {
        self.line(&format!("set_date unix_secs={unix_secs}"));
        self.core.set_date(unix_secs);
    }

    /// The core's static roots.
    pub fn static_roots(&self) -> &[PathBuf] {
        self.core.static_roots()
    }

    /// A new client connection from `peer`; delivers `Accepted` at once.
    pub fn connect(&mut self, peer: IpAddr) -> ConnId {
        let conn = ConnId(self.next_id);
        self.next_id += 1;
        self.conns.insert(
            conn,
            SimConn {
                to_server: VecDeque::new(),
                eof: Eof::NotSent,
                client: ClientSide::Live,
                received: Vec::new(),
                read_chunk: usize::MAX,
                window: Window::Unlimited,
                interest: Interest::None,
                state: SimConnState::Open,
                send_capacity: 0,
            },
        );
        self.deliver(Event::Accepted { conn, peer });
        let send_capacity = self.core.send_capacity(conn);
        self.conn_mut(conn).send_capacity = send_capacity;
        conn
    }

    /// Queue `bytes` from the client to the server.
    pub fn send(&mut self, conn: ConnId, bytes: &[u8]) {
        let c = self.conn_mut(conn);
        assert_eq!(c.eof, Eof::NotSent, "send after send_eof on {conn:?}");
        c.to_server.extend(bytes);
    }

    /// Queue end of stream from the client, after any queued bytes.
    pub fn send_eof(&mut self, conn: ConnId) {
        let c = self.conn_mut(conn);
        if c.eof == Eof::NotSent {
            c.eof = Eof::Queued;
        }
    }

    /// The client resets the connection; delivers `Failed` at once (unless
    /// the server already closed it).
    pub fn reset(&mut self, conn: ConnId) {
        let c = self.conn_mut(conn);
        if c.client == ClientSide::Reset || c.state == SimConnState::Closed {
            return;
        }
        c.client = ClientSide::Reset;
        c.to_server.clear();
        self.deliver(Event::Failed {
            conn,
            error: ConnError::Reset,
        });
    }

    /// Deliver at most `max` bytes per `Received`.
    pub fn set_read_chunk(&mut self, conn: ConnId, max: usize) {
        assert!(max > 0, "read chunk must be positive");
        self.conn_mut(conn).read_chunk = max;
    }

    /// How many bytes the client accepts per `Sent`.
    pub fn set_client_window(&mut self, conn: ConnId, window: Window) {
        self.conn_mut(conn).window = window;
    }

    /// Pump queued bytes both ways until no connection makes progress.
    pub fn run_until_idle(&mut self) {
        let mut steps: usize = 0;
        while self.pump_once() {
            steps += 1;
            if steps > self.step_limit {
                self.violation(&format!(
                    "run_until_idle exceeded {} steps",
                    self.step_limit
                ));
            }
        }
        self.check_upstreams_settled();
        let stuck = self.conns.iter().find(|(id, c)| {
            c.state != SimConnState::Closed
                && c.interest != Interest::Write
                && (!self.core.send_buffer(**id).is_empty() || self.core.send_file(**id).is_some())
        });
        if let Some((conn, c)) = stuck {
            self.violation(&format!(
                "conn={} has pending send bytes but interest stays {:?}",
                conn.0, c.interest
            ));
        }
    }

    /// One pass over all connections; returns whether anything moved.
    fn pump_once(&mut self) -> bool {
        let mut progress = self.pump_disk();
        progress |= self.pump_upstreams();
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            let c = &self.conns[&conn];
            if c.state == SimConnState::Closed || c.client == ClientSide::Reset {
                continue;
            }
            match c.interest {
                Interest::None => {}
                Interest::Read => progress |= self.pump_read(conn),
                Interest::Write => progress |= self.pump_write(conn),
                // Never set: `apply` rejects it.
                Interest::ReadWrite => {}
            }
        }
        progress
    }

    fn pump_read(&mut self, conn: ConnId) -> bool {
        let c = self.conns.get_mut(&conn).unwrap();
        if !c.to_server.is_empty() {
            return self.deliver_read(conn);
        }
        if c.eof == Eof::Queued {
            c.eof = Eof::Delivered;
            self.deliver(Event::ReadClosed { conn });
            return true;
        }
        false
    }

    /// The only path that writes into `recv_buffer`: copies as many queued
    /// client bytes as allowed and reports them with `Received` right away.
    fn deliver_read(&mut self, conn: ConnId) -> bool {
        let c = self.conns.get_mut(&conn).unwrap();
        let spare = self.core.recv_buffer(conn);
        let n = spare.len().min(c.read_chunk).min(c.to_server.len());
        if n == 0 {
            return false;
        }
        for (dst, src) in spare.iter_mut().zip(c.to_server.drain(..n)) {
            *dst = src;
        }
        self.pending_read = Some(Pending::Conn(conn));
        self.deliver(Event::Received { conn, len: n });
        true
    }

    fn window(&self, conn: ConnId) -> usize {
        self.conn(conn).window.size()
    }

    /// The client's window took `n` bytes.
    fn take_window(&mut self, conn: ConnId, n: usize) {
        self.conn_mut(conn).window.take(n);
    }

    fn pump_write(&mut self, conn: ConnId) -> bool {
        let window = self.window(conn);
        let c = self.conns.get_mut(&conn).unwrap();
        let pending = self.core.send_buffer(conn);
        if pending.is_empty() {
            return match self.core.send_file(conn) {
                Some(segment) => self.pump_sendfile(conn, segment),
                None => false,
            };
        }
        let n = pending.len().min(window);
        if n == 0 {
            return false;
        }
        if c.state == SimConnState::WriteShutdown {
            self.violation(&format!("conn={} writes after ShutdownWrite", conn.0));
        }
        c.window.take(n);
        c.received.extend_from_slice(&pending[..n]);
        self.deliver(Event::Sent { conn, len: n });
        true
    }

    /// Move the clock forward by `by`, firing every deadline and completing
    /// every disk operation on the way, in time order (disk first on a tie).
    pub fn advance(&mut self, by: Duration) {
        let target = self.now.saturating_add(by);
        loop {
            let timer = self.core.next_deadline();
            let disk = self.disk.front().map(|(due, _)| *due);
            let next = match (timer, disk) {
                (Some(t), Some(d)) => t.min(d),
                (Some(t), None) => t,
                (None, Some(d)) => d,
                (None, None) => break,
            };
            if next > target {
                break;
            }
            self.now = self.now.max(next);
            if disk.is_some_and(|d| d <= self.now) {
                self.run_until_idle();
                continue;
            }
            self.deliver(Event::TimerFired);
            if let Some(next) = self.core.next_deadline() {
                if next <= self.now {
                    self.violation(&format!(
                        "after TimerFired at t={}ns next_deadline is t={}ns, not later",
                        self.now.as_nanos(),
                        next.as_nanos()
                    ));
                }
            }
            self.run_until_idle();
        }
        self.now = target;
    }

    /// The simulated current time.
    pub fn now(&self) -> Instant {
        self.now
    }

    /// Take the bytes the client has received so far on `conn`.
    pub fn take_received(&mut self, conn: ConnId) -> Vec<u8> {
        std::mem::take(&mut self.conn_mut(conn).received)
    }

    /// Client bytes not yet delivered to the server.
    pub fn queued_to_server(&self, conn: ConnId) -> usize {
        self.conn(conn).to_server.len()
    }

    /// The interest the core last set for `conn`.
    pub fn interest(&self, conn: ConnId) -> Interest {
        self.conn(conn).interest
    }

    /// The network-side state of `conn`.
    pub fn state(&self, conn: ConnId) -> SimConnState {
        self.conn(conn).state
    }

    /// Every connection id the sim has handed out, in order.
    pub fn conn_ids(&self) -> Vec<ConnId> {
        self.conns.keys().copied().collect()
    }

    /// Whether the core still holds state for `conn` (it reports buffer
    /// capacity only for connections it knows).
    pub fn core_knows(&self, conn: ConnId) -> bool {
        self.core.send_capacity(conn) > 0 || self.core.recv_capacity(conn) > 0
    }

    /// Every log entry the core has emitted, in order.
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }

    /// One line per event and action, in order.
    pub fn trace(&self) -> &str {
        &self.trace
    }

    fn conn(&self, conn: ConnId) -> &SimConn {
        match self.conns.get(&conn) {
            Some(c) => c,
            None => panic!("unknown sim connection {conn:?}"),
        }
    }

    fn conn_mut(&mut self, conn: ConnId) -> &mut SimConn {
        match self.conns.get_mut(&conn) {
            Some(c) => c,
            None => panic!("unknown sim connection {conn:?}"),
        }
    }

    fn violation(&self, what: &str) -> ! {
        panic!("sim invariant violated: {what}\ntrace:\n{}", self.trace);
    }

    fn line(&mut self, text: &str) {
        let _ = writeln!(self.trace, "t={}ns {text}", self.now.as_nanos());
    }

    /// Hand one event to the core and apply the resulting actions.
    fn deliver(&mut self, event: Event) {
        self.line(&event_text(&event));
        if let Some(pending) = self.pending_read.take() {
            let reported = match &event {
                Event::Received { conn, .. } => Some(Pending::Conn(*conn)),
                Event::UpstreamReceived { up, .. } => Some(Pending::Upstream(*up)),
                Event::Accepted { .. }
                | Event::ReadClosed { .. }
                | Event::Sent { .. }
                | Event::Failed { .. }
                | Event::TimerFired
                | Event::FileOpened { .. }
                | Event::FileFailed { .. }
                | Event::ChunkReady { .. }
                | Event::FileSent { .. }
                | Event::UpstreamConnected { .. }
                | Event::UpstreamFailed { .. }
                | Event::UpstreamReadClosed { .. }
                | Event::UpstreamSent { .. } => None,
            };
            if reported != Some(pending) {
                let what = match pending {
                    Pending::Conn(conn) => format!("recv_buffer of conn={}", conn.0),
                    Pending::Upstream(up) => format!("upstream_recv_buffer of up={}", up.0),
                };
                self.violation(&format!(
                    "read into {what} not reported by its Received before the next event"
                ));
            }
        }
        self.context = match &event {
            Event::Accepted { conn, .. }
            | Event::Received { conn, .. }
            | Event::ReadClosed { conn }
            | Event::Sent { conn, .. }
            | Event::Failed { conn, .. }
            | Event::FileSent { conn, .. } => Some(*conn),
            Event::FileOpened { file, .. }
            | Event::FileFailed { file, .. }
            | Event::ChunkReady { file, .. } => self.files.get(file).map(|t| t.conn),
            Event::UpstreamConnected { up }
            | Event::UpstreamFailed { up, .. }
            | Event::UpstreamReceived { up, .. }
            | Event::UpstreamReadClosed { up }
            | Event::UpstreamSent { up, .. } => self.backends.conn_of(*up),
            Event::TimerFired => None,
        };
        let mut actions = std::mem::take(&mut self.actions);
        actions.clear();
        self.core.handle(self.now, event, &mut actions);
        for action in actions.drain(..) {
            self.apply(action);
        }
        self.actions = actions;
        self.context = None;
        self.check_buffers();
        self.check_files();
        self.check_upstreams();
    }

    fn apply(&mut self, action: Action) {
        self.line(&action_text(&action));
        let conn = match &action {
            Action::SetInterest { conn, .. }
            | Action::ShutdownWrite { conn }
            | Action::Close { conn } => *conn,
            Action::Log(log) => log_conn(log),
            Action::OpenFile { .. } | Action::Readahead { .. } | Action::CloseFile { .. } => {
                return self.apply_file(action);
            }
            Action::Connect { .. }
            | Action::SetUpstreamInterest { .. }
            | Action::CloseUpstream { .. } => return self.apply_upstream(action),
        };
        let state = match self.conns.get(&conn) {
            Some(c) => c.state,
            None => self.violation(&format!("action for unknown conn={}", conn.0)),
        };
        if state == SimConnState::Closed {
            self.violation(&format!("action after Close on conn={}", conn.0));
        }
        let c = self.conns.get_mut(&conn).unwrap();
        match action {
            Action::SetInterest {
                interest: Interest::ReadWrite,
                ..
            } => self.violation(&format!(
                "SetInterest ReadWrite for conn={}: it is for upstreams only",
                conn.0
            )),
            Action::SetInterest { interest, .. } => c.interest = interest,
            Action::ShutdownWrite { .. } => c.state = SimConnState::WriteShutdown,
            Action::Close { .. } => {
                c.state = SimConnState::Closed;
                c.interest = Interest::None;
            }
            Action::Log(log) => self.logs.push(log),
            Action::OpenFile { .. }
            | Action::Readahead { .. }
            | Action::CloseFile { .. }
            | Action::Connect { .. }
            | Action::SetUpstreamInterest { .. }
            | Action::CloseUpstream { .. } => {}
        }
    }

    /// Buffers stay within their capacities. The receive capacity is the
    /// core's current report (it changes on entering a lingering close);
    /// the send capacity is the one recorded at accept, or, once the
    /// connection has proxied, one of the exchange's buffer capacities the
    /// config implies.
    fn check_buffers(&mut self) {
        let open: Vec<(ConnId, Interest, usize)> = self
            .conns
            .iter()
            .filter(|(_, c)| c.state != SimConnState::Closed && c.send_capacity > 0)
            .map(|(id, c)| (*id, c.interest, c.send_capacity))
            .collect();
        for (conn, interest, accepted) in open {
            let send_cap = self.core.send_capacity(conn);
            let allowed = if self.ever_proxied(conn) {
                [accepted, self.backends.head_bound, self.backends.recv_bound].contains(&send_cap)
            } else {
                send_cap == accepted
            };
            if !allowed {
                self.violation(&format!(
                    "conn={} send capacity {send_cap} is not one it may have \
                     (accepted with {accepted})",
                    conn.0
                ));
            }
            let recv_cap = self.core.recv_capacity(conn);
            let recv = self.core.recv_buffer(conn).len();
            if recv > recv_cap {
                self.violation(&format!(
                    "conn={} recv_buffer {recv} bytes exceeds capacity {recv_cap}",
                    conn.0
                ));
            }
            if interest == Interest::Read && recv == 0 {
                self.violation(&format!(
                    "conn={} wants to read but recv_buffer is empty",
                    conn.0
                ));
            }
            let pending = self.core.request_body_pending(conn);
            self.check_request_body(conn, recv, recv_cap, pending);
            let send = self.core.send_buffer(conn).len();
            if send > send_cap {
                self.violation(&format!(
                    "conn={} send_buffer {send} bytes exceeds capacity {send_cap}",
                    conn.0
                ));
            }
        }
    }
}

impl Sim {
    /// Fill, drain, refill on the client side of a request body
    /// (NFR-RES-04): while `conn` forwards a body (`pending`, the core's
    /// report), a read is offered only once no body byte of its buffer is
    /// unsent, and then with all of the buffer's `capacity` but a
    /// chunked-framing tail of at most `MAX_UNCONSUMED` bytes kept at its
    /// front.
    pub(crate) fn check_request_body(
        &self,
        conn: ConnId,
        offered: usize,
        capacity: usize,
        pending: Option<BodyPending>,
    ) {
        let Some(BodyPending { unsent, tail }) = pending else {
            return;
        };
        if tail > MAX_UNCONSUMED {
            self.violation(&format!(
                "conn={} keeps a chunked tail of {tail} bytes, more than {MAX_UNCONSUMED}",
                conn.0
            ));
        }
        if offered == 0 {
            return;
        }
        if unsent > 0 {
            self.violation(&format!(
                "conn={} offers a read while {unsent} request body bytes are unsent",
                conn.0
            ));
        }
        if offered + tail != capacity {
            self.violation(&format!(
                "conn={} offers a body read of {offered} bytes behind a {tail}-byte tail, \
                 not the rest of its capacity {capacity}",
                conn.0
            ));
        }
    }
}

fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. }
        | Log::ProxiedResponse { conn, .. }
        | Log::BackendFailed { conn, .. } => *conn,
    }
}

fn event_text(event: &Event) -> String {
    match event {
        Event::Accepted { conn, peer } => format!("conn={} Accepted peer={peer}", conn.0),
        Event::Received { conn, len } => format!("conn={} Received len={len}", conn.0),
        Event::ReadClosed { conn } => format!("conn={} ReadClosed", conn.0),
        Event::Sent { conn, len } => format!("conn={} Sent len={len}", conn.0),
        Event::Failed { conn, error } => format!("conn={} Failed error={error:?}", conn.0),
        Event::TimerFired => "TimerFired".to_owned(),
        Event::FileOpened { file, meta } => format!(
            "file={} FileOpened kind={:?} size={} modified_ns={}",
            file.0, meta.kind, meta.stamp.size, meta.stamp.modified_ns
        ),
        Event::FileFailed { file, error } => {
            format!("file={} FileFailed error={error:?}", file.0)
        }
        Event::ChunkReady { file, stamp } => format!(
            "file={} ChunkReady size={} modified_ns={}",
            file.0, stamp.size, stamp.modified_ns
        ),
        Event::FileSent { conn, len } => format!("conn={} FileSent len={len}", conn.0),
        Event::UpstreamConnected { up } => format!("up={} UpstreamConnected", up.0),
        Event::UpstreamFailed { up, error } => {
            format!("up={} UpstreamFailed error={error:?}", up.0)
        }
        Event::UpstreamReceived { up, len } => format!("up={} UpstreamReceived len={len}", up.0),
        Event::UpstreamReadClosed { up } => format!("up={} UpstreamReadClosed", up.0),
        Event::UpstreamSent { up, len } => format!("up={} UpstreamSent len={len}", up.0),
    }
}

fn action_text(action: &Action) -> String {
    match action {
        Action::SetInterest { conn, interest } => {
            format!("conn={} -> SetInterest {interest:?}", conn.0)
        }
        Action::ShutdownWrite { conn } => format!("conn={} -> ShutdownWrite", conn.0),
        Action::Close { conn } => format!("conn={} -> Close", conn.0),
        Action::Log(log) => format!("conn={} -> Log {log:?}", log_conn(log).0),
        Action::OpenFile { file, root, path } => {
            let path: Vec<String> = path
                .iter()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            format!("file={} -> OpenFile root={} path={path:?}", file.0, root.0)
        }
        Action::Readahead { file, offset, len } => {
            format!("file={} -> Readahead offset={offset} len={len}", file.0)
        }
        Action::CloseFile { file } => format!("file={} -> CloseFile", file.0),
        Action::Connect { up, backend } => {
            format!("up={} -> Connect backend={}", up.0, backend.0)
        }
        Action::SetUpstreamInterest { up, interest } => {
            format!("up={} -> SetUpstreamInterest {interest:?}", up.0)
        }
        Action::CloseUpstream { up } => format!("up={} -> CloseUpstream", up.0),
    }
}

#[cfg(test)]
mod tests;
