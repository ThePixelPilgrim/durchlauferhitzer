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

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::time::Duration;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{Action, ConnError, ConnId, Core, Event, Instant, Interest, Log};

/// Capacity of the core's send buffer (mirrors the core's constant).
const SEND_CAPACITY: usize = 1024;

/// How many bytes the simulated client accepts per `Sent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Unlimited,
    Bytes(usize),
    Stalled,
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
}

/// The simulated network, clock and client population.
#[derive(Debug)]
pub struct Sim {
    core: Core,
    now: Instant,
    next_id: u64,
    recv_capacity: usize,
    conns: BTreeMap<ConnId, SimConn>,
    logs: Vec<Log>,
    trace: String,
    actions: Vec<Action>,
}

impl Sim {
    /// A simulation of a server loaded from `config`. Panics on an invalid
    /// config.
    pub fn new(config: &str) -> Sim {
        let config = match load(config) {
            Ok(c) => c,
            Err(e) => panic!("invalid sim config: {e:?}"),
        };
        let limits = &config.limits;
        let recv_capacity = limits
            .max_head
            .max(limits.max_request_line.saturating_add(2));
        Sim {
            core: Core::new(config),
            now: Instant::from_nanos(0),
            next_id: 1,
            recv_capacity,
            conns: BTreeMap::new(),
            logs: Vec::new(),
            trace: String::new(),
            actions: Vec::new(),
        }
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
            },
        );
        self.deliver(Event::Accepted { conn, peer });
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
        while self.pump_once() {}
        let stuck = self.conns.iter().find(|(id, c)| {
            c.state != SimConnState::Closed
                && c.interest == Interest::Read
                && !self.core.send_buffer(**id).is_empty()
        });
        if let Some((conn, _)) = stuck {
            self.violation(&format!(
                "conn={} has pending send bytes but interest stays Read",
                conn.0
            ));
        }
    }

    /// One pass over all connections; returns whether anything moved.
    fn pump_once(&mut self) -> bool {
        let mut progress = false;
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
            }
        }
        progress
    }

    fn pump_read(&mut self, conn: ConnId) -> bool {
        let c = self.conns.get_mut(&conn).unwrap();
        if !c.to_server.is_empty() {
            let spare = self.core.recv_buffer(conn);
            let n = spare.len().min(c.read_chunk).min(c.to_server.len());
            if n == 0 {
                return false;
            }
            for (dst, src) in spare.iter_mut().zip(c.to_server.drain(..n)) {
                *dst = src;
            }
            self.deliver(Event::Received { conn, len: n });
            return true;
        }
        if c.eof == Eof::Queued {
            c.eof = Eof::Delivered;
            self.deliver(Event::ReadClosed { conn });
            return true;
        }
        false
    }

    fn pump_write(&mut self, conn: ConnId) -> bool {
        let c = self.conns.get_mut(&conn).unwrap();
        let pending = self.core.send_buffer(conn);
        let window = match c.window {
            Window::Unlimited => usize::MAX,
            Window::Bytes(n) => n,
            Window::Stalled => 0,
        };
        let n = pending.len().min(window);
        if n == 0 {
            return false;
        }
        c.received.extend_from_slice(&pending[..n]);
        self.deliver(Event::Sent { conn, len: n });
        true
    }

    /// Move the clock forward by `by`, firing every deadline on the way.
    pub fn advance(&mut self, by: Duration) {
        let target = self.now.saturating_add(by);
        while let Some(deadline) = self.core.next_deadline() {
            if deadline > target {
                break;
            }
            self.now = self.now.max(deadline);
            self.deliver(Event::TimerFired);
            self.run_until_idle();
            if self.core.next_deadline() == Some(deadline) {
                self.violation(&format!(
                    "deadline t={}ns did not move after TimerFired",
                    deadline.as_nanos()
                ));
            }
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
        let mut actions = std::mem::take(&mut self.actions);
        actions.clear();
        self.core.handle(self.now, event, &mut actions);
        for action in actions.drain(..) {
            self.apply(action);
        }
        self.actions = actions;
        self.check_buffers();
    }

    fn apply(&mut self, action: Action) {
        self.line(&action_text(&action));
        let conn = match &action {
            Action::SetInterest { conn, .. }
            | Action::ShutdownWrite { conn }
            | Action::Close { conn } => *conn,
            Action::Log(log) => log_conn(log),
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
            Action::SetInterest { interest, .. } => c.interest = interest,
            Action::ShutdownWrite { .. } => c.state = SimConnState::WriteShutdown,
            Action::Close { .. } => {
                c.state = SimConnState::Closed;
                c.interest = Interest::None;
            }
            Action::Log(log) => self.logs.push(log),
        }
    }

    fn check_buffers(&mut self) {
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            let recv = self.core.recv_buffer(conn).len();
            if recv > self.recv_capacity {
                self.violation(&format!(
                    "conn={} recv_buffer {recv} bytes exceeds capacity {}",
                    conn.0, self.recv_capacity
                ));
            }
            let send = self.core.send_buffer(conn).len();
            if send > SEND_CAPACITY {
                self.violation(&format!(
                    "conn={} send_buffer {send} bytes exceeds capacity {SEND_CAPACITY}",
                    conn.0
                ));
            }
        }
    }
}

fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. } | Log::Rejected { conn, .. } | Log::Reset { conn, .. } => *conn,
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
    }
}
