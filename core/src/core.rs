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
//! # Deadlines
//!
//! Every connection state carries one deadline, set on entry:
//!
//! - reading a request head: `header_timeout`, a *total* deadline from
//!   accept or from the first byte of the next request; trickling bytes do
//!   not extend it. On expiry the core answers 408 if any bytes arrived and
//!   closes silently otherwise;
//! - idle between keep-alive requests: `keepalive_timeout`, then a silent
//!   close;
//! - writing a response: `send_idle_timeout`, reset whenever the socket
//!   accepts bytes; on expiry the connection is closed without further
//!   writes;
//! - lingering after a shutdown of the sending side: [`LINGER_TIMEOUT`],
//!   not extended by input.
//!
//! # Connection limits
//!
//! A connection accepted while `max_connections` connections, or
//! `max_connections_per_ip` from the same peer, are open is answered 503 and
//! closed (with a lingering close). Refused connections do not count toward
//! either limit. Peers are counted by their canonical address, so an
//! IPv4-mapped IPv6 peer shares the slot of its IPv4 address.
//!
//! Resource policy: memory stays bounded by the limits even under a flood of
//! refused connections.
//!
//! - A refused connection gets no receive buffer, only the send buffer that
//!   holds its 503.
//! - At most `max_connections` refused connections are kept at once; beyond
//!   that a new one is logged as rejected and closed at once, with nothing
//!   written.
//! - A connection entering the lingering close drops its own receive buffer;
//!   all lingering connections discard their input into one shared scratch
//!   buffer of [`LINGER_SCRATCH`] bytes, whose contents are never inspected.
//!
//! The core tolerates driver mistakes without panicking: events for unknown
//! connections are ignored, a duplicate `Accepted` is ignored, and a
//! `Received`/`Sent` length larger than the space offered is clamped.

use std::collections::BTreeMap;
use std::net::IpAddr;

use crate::config::Config;
use crate::conn::{
    Conn, ConnState, Counted, LINGER_SCRATCH, LINGER_TIMEOUT, SEND_CAPACITY, UnreadBody,
};
use crate::http::request::{
    BodyFraming, HeadLimits, HeadParser, HttpError, Method, ParseStatus, Persistence, RequestHead,
};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{LimitKind, Log, RejectReason, RequestSummary, TimeoutKind};
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
    /// Open connections that count toward the limits.
    counted: u32,
    /// Counted connections per canonical peer address; entries are removed
    /// at zero.
    counted_per_ip: BTreeMap<IpAddr, u32>,
    /// Refused connections not yet closed; at most `max_connections`.
    refused: u32,
    /// Where every lingering connection discards its input.
    scratch: Box<[u8]>,
}

impl Core {
    /// A core serving `config`, with no connections.
    pub fn new(config: Config) -> Core {
        Core {
            router: Router::new(&config.hosts),
            head_limits: HeadLimits::from(&config.limits),
            config,
            conns: BTreeMap::new(),
            counted: 0,
            counted_per_ip: BTreeMap::new(),
            refused: 0,
            scratch: vec![0; LINGER_SCRATCH].into_boxed_slice(),
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
        match event {
            Event::Accepted { conn, peer } => self.accepted(now, conn, peer, actions),
            Event::Received { conn, len } => self.received(now, conn, len, actions),
            Event::ReadClosed { conn } => {
                if self.forget(conn).is_some() {
                    actions.push(Action::Close { conn });
                }
            }
            Event::Sent { conn, len } => self.sent(now, conn, len, actions),
            Event::Failed { conn, error: _ } => {
                if let Some(c) = self.forget(conn) {
                    match c.state {
                        // A reset while discarding input after our response is
                        // the expected end of a lingering close, not news.
                        ConnState::Lingering { .. } => {}
                        ConnState::ReadingHead { .. }
                        | ConnState::Idle { .. }
                        | ConnState::Responding { .. } => {
                            actions.push(Action::Log(Log::Reset { conn, peer: c.peer }));
                        }
                    }
                    actions.push(Action::Close { conn });
                }
            }
            Event::TimerFired => self.timer_fired(now, actions),
        }
    }

    /// Remove `conn` from the map and release its share of the limits.
    fn forget(&mut self, conn: ConnId) -> Option<Conn> {
        let c = self.conns.remove(&conn)?;
        match c.counted {
            Counted::Yes => {
                self.counted = self.counted.saturating_sub(1);
                let peer = c.peer.to_canonical();
                if let Some(n) = self.counted_per_ip.get_mut(&peer) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.counted_per_ip.remove(&peer);
                    }
                }
            }
            Counted::No => self.refused = self.refused.saturating_sub(1),
        }
        Some(c)
    }

    fn accepted(&mut self, now: Instant, conn: ConnId, peer: IpAddr, actions: &mut Vec<Action>) {
        if self.conns.contains_key(&conn) {
            return;
        }
        let limits = &self.config.limits;
        let canonical = peer.to_canonical();
        let per_ip = self.counted_per_ip.get(&canonical).copied().unwrap_or(0);
        let refusal = if self.counted >= limits.max_connections {
            Some(LimitKind::Connections)
        } else if per_ip >= limits.max_connections_per_ip {
            Some(LimitKind::ConnectionsPerIp)
        } else {
            None
        };
        let capacity = self.new_recv_capacity();
        let c = match refusal {
            None => {
                self.counted = self.counted.saturating_add(1);
                self.counted_per_ip
                    .insert(canonical, per_ip.saturating_add(1));
                let state = ConnState::ReadingHead {
                    buffered: 0,
                    parser: HeadParser::new(),
                    deadline: now.saturating_add(limits.header_timeout),
                };
                Conn::new(peer, Counted::Yes, state, capacity)
            }
            Some(kind) => {
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer,
                    reason: RejectReason::Limit(kind),
                }));
                // Too many refusals in flight: do not even answer.
                if self.refused >= limits.max_connections {
                    actions.push(Action::Close { conn });
                    return;
                }
                self.refused = self.refused.saturating_add(1);
                let state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered: 0,
                    deadline: now.saturating_add(limits.send_idle_timeout),
                };
                // Refused connections never read while responding and then
                // linger into the shared scratch buffer: no buffer of their
                // own.
                let mut c = Conn::new(peer, Counted::No, state, 0);
                render_fixed(
                    Status::ServiceUnavailable,
                    Persistence::Close,
                    BodyMode::Full,
                    &mut c.send,
                );
                c
            }
        };
        self.conns.insert(conn, c);
        self.sync_interest(conn, actions);
    }

    fn received(&mut self, now: Instant, conn: ConnId, len: usize, actions: &mut Vec<Action>) {
        let header_timeout = self.config.limits.header_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let capacity = c.recv.len();
        // A length beyond the spare space is a driver bug: clamp.
        match c.state {
            ConnState::ReadingHead {
                buffered,
                parser,
                deadline,
            } => {
                let added = len.min(capacity.saturating_sub(buffered));
                if added == 0 {
                    return;
                }
                // The header deadline is total: new bytes do not extend it.
                c.state = ConnState::ReadingHead {
                    buffered: buffered.saturating_add(added),
                    parser,
                    deadline,
                };
            }
            ConnState::Idle { .. } => {
                let added = len.min(capacity);
                if added == 0 {
                    return;
                }
                c.state = ConnState::ReadingHead {
                    buffered: added,
                    parser: HeadParser::new(),
                    deadline: now.saturating_add(header_timeout),
                };
            }
            // Input after our response: discarded, the buffer is reused. It
            // does not extend the linger deadline.
            ConnState::Lingering { .. } => return,
            // Not reading: nothing was offered, so nothing can have arrived.
            ConnState::Responding { .. } => return,
        }
        self.try_parse(now, conn, actions);
        self.sync_interest(conn, actions);
    }

    fn sent(&mut self, now: Instant, conn: ConnId, len: usize, actions: &mut Vec<Action>) {
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let len = len.min(c.send.len());
        c.send.drain(..len);
        if let ConnState::Responding {
            persistence,
            unread_body,
            buffered,
            deadline,
        } = c.state
        {
            c.state = if !c.send.is_empty() {
                // Progress resets the send idle deadline.
                let deadline = if len > 0 {
                    now.saturating_add(limits.send_idle_timeout)
                } else {
                    deadline
                };
                ConnState::Responding {
                    persistence,
                    unread_body,
                    buffered,
                    deadline,
                }
            } else {
                match (persistence, unread_body) {
                    (Persistence::KeepAlive, UnreadBody::None) => {
                        if buffered > 0 {
                            ConnState::ReadingHead {
                                buffered,
                                parser: HeadParser::new(),
                                deadline: now.saturating_add(limits.header_timeout),
                            }
                        } else {
                            ConnState::Idle {
                                deadline: now.saturating_add(limits.keepalive_timeout),
                            }
                        }
                    }
                    (Persistence::Close, UnreadBody::None | UnreadBody::Pending)
                    | (Persistence::KeepAlive, UnreadBody::Pending) => {
                        actions.push(Action::ShutdownWrite { conn });
                        // Lingering input goes to the shared scratch buffer.
                        c.recv = Box::default();
                        ConnState::Lingering {
                            deadline: now.saturating_add(LINGER_TIMEOUT),
                        }
                    }
                }
            };
        }
        self.try_parse(now, conn, actions);
        self.sync_interest(conn, actions);
    }

    /// Handle every connection whose deadline is at or before `now`, in
    /// `ConnId` order, until none is left.
    fn timer_fired(&mut self, now: Instant, actions: &mut Vec<Action>) {
        // Each pass closes a connection or moves it from reading a head to
        // responding (which can only expire once more), so this terminates.
        loop {
            let expired: Vec<ConnId> = self
                .conns
                .iter()
                .filter(|(_, c)| c.state.deadline() <= now)
                .map(|(id, _)| *id)
                .collect();
            if expired.is_empty() {
                return;
            }
            for conn in expired {
                self.expire(now, conn, actions);
            }
        }
    }

    /// `conn`'s deadline has passed.
    fn expire(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let send_idle_timeout = self.config.limits.send_idle_timeout;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        match c.state {
            // Part of a head arrived, too slowly: answer 408.
            ConnState::ReadingHead { buffered, .. } if buffered > 0 => {
                render_fixed(
                    Status::RequestTimeout,
                    Persistence::Close,
                    BodyMode::Full,
                    &mut c.send,
                );
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(TimeoutKind::Header),
                }));
                c.state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered: 0,
                    deadline: now.saturating_add(send_idle_timeout),
                };
                self.sync_interest(conn, actions);
            }
            // Nothing arrived, or nothing is left to say: close silently.
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Lingering { .. } => {
                self.forget(conn);
                actions.push(Action::Close { conn });
            }
            // The client stopped reading: close without further writes.
            ConnState::Responding { .. } => {
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(TimeoutKind::Send),
                }));
                self.forget(conn);
                actions.push(Action::Close { conn });
            }
        }
    }

    /// In `ReadingHead`, parse the buffered bytes. The parser resumes where
    /// it stopped, so only newly arrived bytes are looked at.
    fn try_parse(&mut self, now: Instant, conn: ConnId, actions: &mut Vec<Action>) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::ReadingHead {
            buffered,
            mut parser,
            deadline,
        } = c.state
        else {
            return;
        };
        let capacity = c.recv.len();
        let filled = c.recv.get(..buffered).unwrap_or_default();
        let full = buffered >= capacity;
        let result = match parser.advance(filled, &self.head_limits) {
            // A full buffer that holds no complete head can never hold one:
            // the head would exceed `max_head`.
            Ok(ParseStatus::Incomplete) if full => Err(HttpError::HeadTooLarge),
            Ok(ParseStatus::Incomplete) => Ok(ParseStatus::Incomplete),
            Ok(ParseStatus::Complete { head, consumed }) => {
                Ok(ParseStatus::Complete { head, consumed })
            }
            Err(error) => Err(error),
        };
        c.state = ConnState::ReadingHead {
            buffered,
            parser,
            deadline,
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
                respond(
                    c,
                    &self.router,
                    conn,
                    head,
                    leftover,
                    send_deadline,
                    actions,
                );
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
                    deadline: send_deadline,
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
            Some(c) => match c.state {
                // Discarded input; the contents are never looked at.
                ConnState::Lingering { .. } => &mut self.scratch,
                ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::Responding { .. } => c.recv_spare(),
            },
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

    /// The most [`Core::recv_buffer`] can currently hand out for `conn`:
    /// its own buffer's capacity, or the shared scratch buffer's while it
    /// lingers; 0 for unknown connections. It can change with the
    /// connection's state. For drivers' invariant checks (the simulation),
    /// not for I/O.
    #[doc(hidden)]
    pub fn recv_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |c| match c.state {
            ConnState::Lingering { .. } => self.scratch.len(),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. } => c.recv.len(),
        })
    }

    /// Capacity of `conn`'s send buffer; 0 for unknown connections.
    /// For drivers' invariant checks (the simulation), not for I/O.
    #[doc(hidden)]
    pub fn send_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |_| SEND_CAPACITY)
    }

    /// The earliest instant at which the core wants [`Event::TimerFired`],
    /// or `None` if it has no connections (every connection has a deadline).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.conns.values().map(|c| c.state.deadline()).min()
    }
}

/// Answer a complete request head on `c`, which then responds until
/// `send_deadline` (unless the socket makes progress). `leftover` bytes at
/// the front of `c.recv` wait for the next request.
fn respond(
    c: &mut Conn,
    router: &Router,
    conn: ConnId,
    head: RequestHead,
    leftover: usize,
    send_deadline: Instant,
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
        deadline: send_deadline,
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
        // Only conn 1 remains, waiting for its head (default 10 s).
        assert_eq!(
            core.next_deadline(),
            Some(now.saturating_add(std::time::Duration::from_secs(10)))
        );
    }

    fn secs(n: u64) -> Instant {
        Instant::from_nanos(0).saturating_add(std::time::Duration::from_secs(n))
    }

    fn accept(core: &mut Core, now: Instant, id: u64, peer: IpAddr) -> Vec<Action> {
        let mut actions = Vec::new();
        let conn = ConnId(id);
        core.handle(now, Event::Accepted { conn, peer }, &mut actions);
        actions
    }

    #[test]
    fn limit_counts_return_to_zero_on_every_close_path() {
        let mut core = core();
        let mut actions = Vec::new();
        let now = secs(0);
        let other = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9));
        for (id, peer) in [(1, PEER), (2, PEER), (3, other), (4, other), (5, other)] {
            accept(&mut core, now, id, peer);
        }
        assert_eq!(core.counted, 5);
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&2));
        // conns 4 and 5: a full buffer of zeros is answered 400 at once.
        for id in [4, 5] {
            let conn = ConnId(id);
            core.handle(now, Event::Received { conn, len: 10 }, &mut actions);
        }
        // conn 5's answer is written: it lingers (deadline 2 s); conn 4's is
        // never taken (send deadline 30 s).
        let conn = ConnId(5);
        core.handle(now, Event::Sent { conn, len: 999 }, &mut actions);
        core.handle(now, Event::ReadClosed { conn: ConnId(1) }, &mut actions);
        let error = ConnError::Reset;
        let conn = ConnId(2);
        core.handle(now, Event::Failed { conn, error }, &mut actions);
        assert_eq!(core.counted, 3);
        assert_eq!(core.counted_per_ip.get(&PEER), None);
        // Lingering expiry.
        core.handle(secs(2), Event::TimerFired, &mut actions);
        assert_eq!(core.counted, 2);
        // conn 3 sent nothing: it times out silently at the header deadline.
        core.handle(secs(10), Event::TimerFired, &mut actions);
        assert_eq!(core.counted, 1);
        // Send idle expiry.
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert_eq!(core.counted, 0);
        assert!(core.counted_per_ip.is_empty());
        assert!(core.conns.is_empty());
        assert_eq!(core.next_deadline(), None);
    }

    /// One counted connection allowed, so at most one refused one. The long
    /// header timeout keeps the counted connection open throughout.
    const CAPPED: &str = "listen 127.0.0.1:8080; \
        limits { max_connections 1; header_timeout 60m; } \
        host a.test { path / { static /srv; } }";

    fn capped() -> Core {
        Core::new(crate::config::load(CAPPED).unwrap())
    }

    fn refused(conn: ConnId) -> Action {
        Action::Log(Log::Rejected {
            conn,
            peer: PEER,
            reason: RejectReason::Limit(LimitKind::Connections),
        })
    }

    #[test]
    fn refused_connection_has_no_receive_buffer() {
        let mut core = capped();
        accept(&mut core, secs(0), 1, PEER);
        accept(&mut core, secs(0), 2, PEER);
        let conn = ConnId(2);
        assert!(!core.send_buffer(conn).is_empty(), "a 503 is waiting");
        assert_eq!(core.recv_capacity(conn), 0);
        assert!(core.recv_buffer(conn).is_empty());
        assert_eq!(core.send_capacity(conn), SEND_CAPACITY);
    }

    #[test]
    fn lingering_connections_share_a_scratch_buffer() {
        let mut core = capped();
        let mut actions = Vec::new();
        let now = secs(0);
        accept(&mut core, now, 1, PEER);
        accept(&mut core, now, 2, PEER);
        // conn 1 (counted): no Host header, answered 400, then lingers.
        let c1 = ConnId(1);
        let request = b"GET / HTTP/1.1\r\n\r\n";
        let spare = core.recv_buffer(c1);
        assert!(spare.len() > 4096);
        if let Some(dst) = spare.get_mut(..request.len()) {
            dst.copy_from_slice(request);
        }
        let len = request.len();
        core.handle(now, Event::Received { conn: c1, len }, &mut actions);
        for id in [1, 2] {
            let conn = ConnId(id);
            core.handle(now, Event::Sent { conn, len: 999 }, &mut actions);
            assert!(actions.contains(&Action::ShutdownWrite { conn }));
            assert_eq!(core.recv_capacity(conn), 4096);
            assert_eq!(core.recv_buffer(conn).len(), 4096);
            assert_eq!(core.conns.get(&conn).map(|c| c.recv.len()), Some(0));
            actions.clear();
            core.handle(now, Event::Received { conn, len: 4096 }, &mut actions);
            assert!(actions.is_empty());
        }
    }

    #[test]
    fn refused_connections_beyond_the_cap_are_closed_at_once() {
        let mut core = capped();
        let now = secs(0);
        accept(&mut core, now, 1, PEER);
        let second = accept(&mut core, now, 2, PEER);
        assert!(!second.contains(&Action::Close { conn: ConnId(2) }));
        let conn = ConnId(3);
        assert_eq!(
            accept(&mut core, now, 3, PEER),
            [refused(conn), Action::Close { conn }]
        );
        assert!(!core.conns.contains_key(&conn));
        assert!(core.send_buffer(conn).is_empty());
        assert_eq!(core.recv_capacity(conn), 0);
    }

    #[test]
    fn refused_cap_is_released_on_every_close_path() {
        let mut core = capped();
        let mut actions = Vec::new();
        accept(&mut core, secs(0), 1, PEER);
        let mut id = 2;
        // Each closing path, applied to a refused connection accepted at `t`,
        // frees the single refused slot for the next round.
        for path in 0..4 {
            let t = secs(100 * path);
            let conn = ConnId(id);
            id += 1;
            let accepted = accept(&mut core, t, conn.0, PEER);
            assert!(!accepted.contains(&Action::Close { conn }), "path {path}");
            let over = ConnId(id);
            id += 1;
            assert_eq!(
                accept(&mut core, t, over.0, PEER),
                [refused(over), Action::Close { conn: over }],
                "path {path}"
            );
            actions.clear();
            match path {
                0 => core.handle(t, Event::ReadClosed { conn }, &mut actions),
                1 => {
                    let error = ConnError::Reset;
                    core.handle(t, Event::Failed { conn, error }, &mut actions);
                }
                2 => {
                    core.handle(t, Event::Sent { conn, len: 999 }, &mut actions);
                    let linger_end = t.saturating_add(LINGER_TIMEOUT);
                    core.handle(linger_end, Event::TimerFired, &mut actions);
                }
                _ => {
                    let stalled = t.saturating_add(std::time::Duration::from_secs(30));
                    core.handle(stalled, Event::TimerFired, &mut actions);
                }
            }
            assert!(actions.contains(&Action::Close { conn }), "path {path}");
            assert_eq!(core.conns.len(), 1, "path {path}");
        }
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

    /// A maximal head sent one line per read costs linear parser work: every
    /// byte is searched for an LF once and judged once, and the finished head
    /// is built once (regression test for rescanning the whole buffer on
    /// every new line).
    #[test]
    fn head_sent_line_by_line_is_parsed_in_linear_work() {
        use crate::http::request::work;
        let mut core = Core::new(
            crate::config::load("listen 127.0.0.1:8080; host a.test { path / { static /srv; } }")
                .unwrap(),
        );
        let limits = core.config.limits.clone();
        let mut lines: Vec<Vec<u8>> =
            vec![b"GET / HTTP/1.1\r\n".to_vec(), b"Host: a.test\r\n".to_vec()];
        let fixed: usize = lines.iter().map(Vec::len).sum();
        let count = limits.max_headers - 1;
        let width = (limits.max_head - fixed - 2) / count;
        for i in 0..count {
            let mut line = format!("X-Pad-{i:03}: ").into_bytes();
            line.resize(width - 2, b'x');
            line.extend_from_slice(b"\r\n");
            lines.push(line);
        }
        lines.push(b"\r\n".to_vec());
        let total: usize = lines.iter().map(Vec::len).sum();
        assert!(total <= limits.max_head && total > limits.max_head - width);

        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        let mut actions = Vec::new();
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        work::take();
        for line in &lines {
            let spare = core.recv_buffer(conn);
            spare.get_mut(..line.len()).unwrap().copy_from_slice(line);
            core.handle(
                now,
                Event::Received {
                    conn,
                    len: line.len(),
                },
                &mut actions,
            );
        }
        let used = work::take();
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Log(Log::Response { .. }))),
            "{actions:?}"
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Log(Log::Rejected { .. }))),
            "{actions:?}"
        );
        assert!(used.scanned <= total, "{used:?} for a {total}-byte head");
        assert!(used.judged <= total, "{used:?} for a {total}-byte head");
        assert!(used.built <= total, "{used:?} for a {total}-byte head");
    }
}
