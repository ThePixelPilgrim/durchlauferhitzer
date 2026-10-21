//! The sans-IO core and its contract with drivers.
//!
//! # Contract
//!
//! The core never touches sockets, clocks or files. A *driver* (the
//! simulation in tests, the epoll driver in production) owns all I/O and talks
//! to the core through exactly three channels:
//!
//! 1. **Events in.** The driver calls [`Core::handle`] with the current time
//!    and one [`Event`]. The core appends zero or more [`Action`]s to the
//!    caller's vector, in the order the driver must perform them.
//! 2. **Receive buffer.** Before reading from a socket the driver asks for
//!    [`Core::recv_buffer`], reads *directly* into the returned slice (no
//!    intermediate copy), and then reports how many bytes it wrote with
//!    [`Event::Received`]. The bytes land in core-owned memory, so per
//!    connection memory is bounded by the buffer's fixed capacity.
//! 3. **Send buffer.** When the connection's interest is
//!    [`Interest::Write`], the driver writes *directly* from
//!    [`Core::send_buffer`] and reports how many bytes the socket accepted
//!    with [`Event::Sent`]; the core then drops those bytes from the front.
//!
//! Rules every driver follows:
//!
//! - The driver allocates [`ConnId`]s and never reuses one, not even after
//!   the connection closed. The first event for a connection is
//!   [`Event::Accepted`].
//! - The driver performs the actions of one `handle` call in order before
//!   delivering the next event. A slice obtained from `recv_buffer` or
//!   `send_buffer` is only valid until the next `handle` call (the borrow
//!   checker enforces this).
//! - After [`Action::Close`] the core forgets the connection; the driver
//!   closes the socket and delivers no further events for that id. The core
//!   emits no further actions for it. Stray I/O events for a forgotten id are
//!   ignored, but an `Accepted` for it creates a *new* connection: the core
//!   cannot tell reuse from a fresh id, so never reusing ids is the driver's
//!   obligation.
//! - Readiness is level-triggered: while interest is [`Interest::Read`], the
//!   driver keeps reading into `recv_buffer` until it is empty (zero spare)
//!   or the socket would block; while interest is [`Interest::Write`], it
//!   keeps writing from `send_buffer` until it is empty or the socket would
//!   block. An edge-triggered driver must loop to `EAGAIN`.
//! - A read into `recv_buffer(c)` is reported by `Received { conn: c, .. }`
//!   before any other `handle` call (or not at all, if nothing was read).
//! - An **empty** `recv_buffer` means "do not read now" (unknown connection,
//!   not in a reading state, or buffer full). An empty `send_buffer` means
//!   "nothing to write". Neither is an error.
//! - The driver arms timers from [`Core::next_deadline`] after every `handle`
//!   call and delivers [`Event::TimerFired`] once that instant has passed.
//!   After `TimerFired`, every deadline at or before `now` has been handled:
//!   `next_deadline` is `None` or later than `now`.
//!
//! The core tolerates driver mistakes without panicking: events for unknown
//! connections are ignored, a duplicate `Accepted` is ignored, and a
//! `Received`/`Sent` length larger than the space offered is clamped.

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::config::Config;
use crate::conn::{Conn, ConnState, SEND_CAPACITY, UnreadBody};
use crate::http::request::{
    BodyFraming, HeadLimits, HttpError, Method, ParseStatus, Persistence, RequestHead, parse_head,
};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RejectReason, RequestSummary};
use crate::routing::Router;
use crate::time::Instant;

/// Opaque connection id, allocated by the driver and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// Transport-level failure reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnError {
    /// The peer reset the connection (`ECONNRESET`, `EPIPE`).
    Reset,
    /// Any other socket error.
    Other,
}

/// Something that happened in the outside world. Each is delivered with the
/// current time via [`Core::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A new client connection was accepted. Always the first event for
    /// `conn`.
    Accepted { conn: ConnId, peer: IpAddr },
    /// The driver wrote `len` bytes into the slice returned by the last
    /// [`Core::recv_buffer`] call for `conn`, starting at its first byte.
    /// `len` is never 0 (end of stream is [`Event::ReadClosed`]). It must be
    /// the very next `handle` call after that read: no other event may come
    /// in between, or the core may have moved or reused that space.
    Received { conn: ConnId, len: usize },
    /// The peer closed its sending side (a read returned end of stream).
    ReadClosed { conn: ConnId },
    /// The socket accepted the first `len` bytes of [`Core::send_buffer`].
    Sent { conn: ConnId, len: usize },
    /// The socket failed; the driver will not do further I/O on it except
    /// what the core's resulting actions ask for (`Close`).
    Failed { conn: ConnId, error: ConnError },
    /// The instant last returned by [`Core::next_deadline`] has passed.
    /// After handling it, every deadline at or before `now` has been
    /// handled: `next_deadline` returns `None` or an instant after `now`.
    TimerFired,
}

/// Which readiness the driver should wait for on a connection. Replaces the
/// previous interest entirely; a new connection starts at `None` until the
/// core says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// Wait for nothing (e.g. receive buffer full: backpressure).
    None,
    /// Read into [`Core::recv_buffer`] when readable.
    Read,
    /// Write from [`Core::send_buffer`] when writable.
    Write,
}

/// Something the driver must do, in the order emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Change what the driver waits for on `conn`.
    SetInterest { conn: ConnId, interest: Interest },
    /// Shut down the sending side of `conn` (half-close); reading may go on.
    ShutdownWrite { conn: ConnId },
    /// Close `conn`. The last action the core emits for it.
    Close { conn: ConnId },
    /// Emit a log entry.
    Log(Log),
}

/// The I/O-free server core. See the module documentation for the contract.
#[derive(Debug)]
pub struct Core {
    config: Config,
    router: Router,
    head_limits: HeadLimits,
    conns: BTreeMap<ConnId, Conn>,
}

impl Core {
    /// A core serving `config`, with no connections.
    pub fn new(config: Config) -> Core {
        Core {
            router: Router::new(&config.hosts),
            head_limits: HeadLimits::from(&config.limits),
            config,
            conns: BTreeMap::new(),
        }
    }

    /// Capacity of each connection's receive buffer: room for the largest
    /// allowed head, and for `max_request_line + 2` bytes so the parser can
    /// always reach a verdict on an over-long request line.
    fn new_recv_capacity(&self) -> usize {
        let limits = &self.config.limits;
        limits
            .max_head
            .max(limits.max_request_line.saturating_add(2))
    }

    /// Process one event at time `now`, appending the resulting actions.
    pub fn handle(&mut self, now: Instant, event: Event, actions: &mut Vec<Action>) {
        // `now` is unused until deadlines arrive in a later task.
        let _ = now;
        match event {
            Event::Accepted { conn, peer } => self.accepted(conn, peer, actions),
            Event::Received { conn, len } => self.received(conn, len, actions),
            Event::ReadClosed { conn } => {
                if self.conns.remove(&conn).is_some() {
                    actions.push(Action::Close { conn });
                }
            }
            Event::Sent { conn, len } => self.sent(conn, len, actions),
            Event::Failed { conn, error: _ } => {
                if let Some(c) = self.conns.remove(&conn) {
                    match c.state {
                        // A reset while discarding input after our response is
                        // the expected end of a lingering close, not news.
                        ConnState::Lingering => {}
                        ConnState::ReadingHead { .. }
                        | ConnState::Idle
                        | ConnState::Responding { .. } => {
                            actions.push(Action::Log(Log::Reset { conn, peer: c.peer }));
                        }
                    }
                    actions.push(Action::Close { conn });
                }
            }
            Event::TimerFired => {}
        }
    }

    fn accepted(&mut self, conn: ConnId, peer: IpAddr, actions: &mut Vec<Action>) {
        if self.conns.contains_key(&conn) {
            return;
        }
        let capacity = self.new_recv_capacity();
        self.conns.insert(conn, Conn::new(peer, capacity));
        self.sync_interest(conn, actions);
    }

    fn received(&mut self, conn: ConnId, len: usize, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let capacity = c.recv.len();
        // A length beyond the spare space is a driver bug: clamp.
        match c.state {
            ConnState::ReadingHead { buffered, scanned } => {
                let added = len.min(capacity.saturating_sub(buffered));
                if added == 0 {
                    return;
                }
                c.state = ConnState::ReadingHead {
                    buffered: buffered.saturating_add(added),
                    scanned,
                };
            }
            ConnState::Idle => {
                let added = len.min(capacity);
                if added == 0 {
                    return;
                }
                c.state = ConnState::ReadingHead {
                    buffered: added,
                    scanned: 0,
                };
            }
            // Input after our response: discarded, the buffer is reused.
            ConnState::Lingering => return,
            // Not reading: nothing was offered, so nothing can have arrived.
            ConnState::Responding { .. } => return,
        }
        self.try_parse(conn, actions);
        self.sync_interest(conn, actions);
    }

    fn sent(&mut self, conn: ConnId, len: usize, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let len = len.min(c.send.len());
        c.send.drain(..len);
        if let ConnState::Responding {
            persistence,
            unread_body,
            buffered,
        } = c.state
        {
            if c.send.is_empty() {
                c.state = match (persistence, unread_body) {
                    (Persistence::KeepAlive, UnreadBody::None) => {
                        if buffered > 0 {
                            ConnState::ReadingHead {
                                buffered,
                                scanned: 0,
                            }
                        } else {
                            ConnState::Idle
                        }
                    }
                    (Persistence::Close, UnreadBody::None | UnreadBody::Pending)
                    | (Persistence::KeepAlive, UnreadBody::Pending) => {
                        actions.push(Action::ShutdownWrite { conn });
                        ConnState::Lingering
                    }
                };
            }
        }
        self.try_parse(conn, actions);
        self.sync_interest(conn, actions);
    }

    /// In `ReadingHead`, parse the buffered bytes if anything new can come of
    /// it: an LF beyond `scanned`, or a full buffer (so a limit fires).
    fn try_parse(&mut self, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::ReadingHead { buffered, scanned } = c.state else {
            return;
        };
        let capacity = c.recv.len();
        let filled = c.recv.get(..buffered).unwrap_or_default();
        let new_lf = filled.get(scanned..).unwrap_or_default().contains(&b'\n');
        c.state = ConnState::ReadingHead {
            buffered,
            scanned: buffered,
        };
        let full = buffered >= capacity;
        if !new_lf && !full {
            return;
        }
        let result = match parse_head(filled, &self.head_limits) {
            // A full buffer that holds no complete head can never hold one:
            // the head would exceed `max_head`.
            Ok(ParseStatus::Incomplete) if full => Err(HttpError::HeadTooLarge),
            other => other,
        };
        match result {
            Ok(ParseStatus::Incomplete) => {}
            Ok(ParseStatus::Complete { head, consumed }) => {
                // Bytes after the head belong to the next request: move them
                // to the front.
                let leftover = buffered.saturating_sub(consumed);
                if consumed <= buffered {
                    c.recv.copy_within(consumed..buffered, 0);
                }
                respond(c, &self.router, conn, head, leftover, actions);
            }
            Err(error) => {
                render_fixed(
                    error.status(),
                    Persistence::Close,
                    BodyMode::Full,
                    &mut c.send,
                );
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Http(error),
                }));
                c.state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered: 0,
                };
            }
        }
    }

    /// Tell the driver if the interest the connection needs has changed.
    fn sync_interest(&mut self, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let interest = c.wanted_interest();
        if interest != c.interest {
            c.interest = interest;
            actions.push(Action::SetInterest { conn, interest });
        }
    }

    /// Writable spare space for `conn`'s next read. Empty when the connection
    /// is unknown, not reading, or its buffer is full.
    ///
    /// If the driver writes any bytes into it, it reports them with
    /// [`Event::Received`] for `conn` as the very next `handle` call.
    pub fn recv_buffer(&mut self, conn: ConnId) -> &mut [u8] {
        match self.conns.get_mut(&conn) {
            Some(c) => c.recv_spare(),
            None => &mut [],
        }
    }

    /// Bytes waiting to be written to `conn`. Empty when there are none or
    /// the connection is unknown.
    pub fn send_buffer(&self, conn: ConnId) -> &[u8] {
        match self.conns.get(&conn) {
            Some(c) => &c.send,
            None => &[],
        }
    }

    /// Total capacity of `conn`'s receive buffer; 0 for unknown connections.
    /// For drivers' invariant checks (the simulation), not for I/O.
    #[doc(hidden)]
    pub fn recv_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |c| c.recv.len())
    }

    /// Capacity of `conn`'s send buffer; 0 for unknown connections.
    /// For drivers' invariant checks (the simulation), not for I/O.
    #[doc(hidden)]
    pub fn send_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |_| SEND_CAPACITY)
    }

    /// The earliest instant at which the core wants [`Event::TimerFired`],
    /// or `None` if it has no deadline. Always `None` for now.
    pub fn next_deadline(&self) -> Option<Instant> {
        None
    }
}

/// Answer a complete request head on `c`, which then responds. `leftover`
/// bytes at the front of `c.recv` wait for the next request.
fn respond(
    c: &mut Conn,
    router: &Router,
    conn: ConnId,
    head: RequestHead,
    leftover: usize,
    actions: &mut Vec<Action>,
) {
    // No handler exists yet in this stage: a matched route is answered 501
    // and the connection closed.
    let (status, persistence) = match router.route(head.host.as_deref(), &head.target.path) {
        Ok(_) => (Status::NotImplemented, Persistence::Close),
        Err(_) => (Status::NotFound, head.persistence),
    };
    let unread_body = match head.body {
        BodyFraming::None => UnreadBody::None,
        BodyFraming::Length(_) | BodyFraming::Chunked => UnreadBody::Pending,
    };
    // An unread body leaves the stream unframed: it cannot be reused.
    let persistence = match unread_body {
        UnreadBody::None => persistence,
        UnreadBody::Pending => Persistence::Close,
    };
    let body = match head.method {
        Method::Head => BodyMode::HeadOnly,
        Method::Get | Method::Other(_) => BodyMode::Full,
    };
    render_fixed(status, persistence, body, &mut c.send);
    actions.push(Action::Log(Log::Response {
        conn,
        peer: c.peer,
        status,
        request: Some(RequestSummary {
            method: head.method,
            host: head.host,
            path: head.target.path,
        }),
    }));
    c.state = ConnState::Responding {
        persistence,
        unread_body,
        buffered: leftover,
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const CONFIG: &str = "listen 127.0.0.1:8080; \
        limits { max_request_line 8; max_head 8; } \
        host a.test { path / { static /srv; } }";

    fn core() -> Core {
        Core::new(crate::config::load(CONFIG).unwrap())
    }

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    #[test]
    fn capacities_are_reported_per_connection() {
        let mut core = core();
        let mut actions = Vec::new();
        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        assert_eq!(core.recv_capacity(conn), 0);
        assert_eq!(core.send_capacity(conn), 0);
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        assert_eq!(core.recv_capacity(conn), 10);
        assert_eq!(core.send_capacity(conn), 1024);
        core.handle(now, Event::Received { conn, len: 4 }, &mut actions);
        assert_eq!(core.recv_capacity(conn), 10);
        core.handle(now, Event::ReadClosed { conn }, &mut actions);
        assert_eq!(core.recv_capacity(conn), 0);
        assert_eq!(core.send_capacity(conn), 0);
    }

    #[test]
    fn oversized_received_is_clamped() {
        let mut core = core();
        let mut actions = Vec::new();
        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        assert_eq!(core.recv_buffer(conn).len(), 10);
        actions.clear();
        core.handle(now, Event::Received { conn, len: 1000 }, &mut actions);
        // Clamped to the 10 spare bytes: the full buffer of zeros is an
        // over-long request line, answered at once.
        assert_eq!(
            actions,
            [
                Action::Log(Log::Rejected {
                    conn,
                    peer: PEER,
                    reason: RejectReason::Http(HttpError::RequestLineTooLong),
                }),
                Action::SetInterest {
                    conn,
                    interest: Interest::Write
                }
            ]
        );
        assert!(core.recv_buffer(conn).is_empty());
        actions.clear();
        core.handle(now, Event::Received { conn, len: 5 }, &mut actions);
        assert!(actions.is_empty());
    }

    #[test]
    fn duplicate_accept_and_unknown_events_are_ignored() {
        let mut core = core();
        let mut actions = Vec::new();
        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        core.handle(now, Event::Received { conn, len: 3 }, &mut actions);
        actions.clear();
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        assert!(actions.is_empty());
        assert_eq!(core.recv_buffer(conn).len(), 7);
        let other = ConnId(2);
        for event in [
            Event::Received {
                conn: other,
                len: 1,
            },
            Event::Sent {
                conn: other,
                len: 1,
            },
            Event::ReadClosed { conn: other },
            Event::Failed {
                conn: other,
                error: ConnError::Other,
            },
            Event::Sent { conn, len: 99 },
            Event::TimerFired,
        ] {
            core.handle(now, event, &mut actions);
        }
        assert!(actions.is_empty());
        assert!(core.recv_buffer(other).is_empty());
        assert!(core.send_buffer(other).is_empty());
        assert_eq!(core.next_deadline(), None);
    }

    #[test]
    fn closed_connection_is_forgotten() {
        let mut core = core();
        let mut actions = Vec::new();
        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        core.handle(now, Event::ReadClosed { conn }, &mut actions);
        assert!(core.conns.is_empty());
        assert!(core.recv_buffer(conn).is_empty());
        actions.clear();
        core.handle(now, Event::ReadClosed { conn }, &mut actions);
        assert!(actions.is_empty());
    }
}
