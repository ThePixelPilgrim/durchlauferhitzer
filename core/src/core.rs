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
//!   closes the socket and delivers no further events for that id (stray
//!   ones are ignored). The core emits no further actions for it.
//! - An **empty** `recv_buffer` means "do not read now" (unknown connection,
//!   not in a reading state, or buffer full). An empty `send_buffer` means
//!   "nothing to write". Neither is an error.
//! - The driver arms timers from [`Core::next_deadline`] after every `handle`
//!   call and delivers [`Event::TimerFired`] once that instant has passed.
//!
//! The core tolerates driver mistakes without panicking: events for unknown
//! connections are ignored, a duplicate `Accepted` is ignored, and a
//! `Received`/`Sent` length larger than the space offered is clamped.

use std::collections::HashMap;
use std::net::IpAddr;

use crate::config::Config;
use crate::conn::{Conn, ConnState};
use crate::log::Log;
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
    /// `len` is never 0 (end of stream is [`Event::ReadClosed`]).
    Received { conn: ConnId, len: usize },
    /// The peer closed its sending side (a read returned end of stream).
    ReadClosed { conn: ConnId },
    /// The socket accepted the first `len` bytes of [`Core::send_buffer`].
    Sent { conn: ConnId, len: usize },
    /// The socket failed; the driver will not do further I/O on it except
    /// what the core's resulting actions ask for (`Close`).
    Failed { conn: ConnId, error: ConnError },
    /// The instant last returned by [`Core::next_deadline`] has passed.
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
    conns: HashMap<ConnId, Conn>,
}

impl Core {
    /// A core serving `config`, with no connections.
    pub fn new(config: Config) -> Core {
        Core {
            config,
            conns: HashMap::new(),
        }
    }

    /// Capacity of each connection's receive buffer: room for the largest
    /// allowed head, and for `max_request_line + 2` bytes so the parser can
    /// always reach a verdict on an over-long request line.
    fn recv_capacity(&self) -> usize {
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
            Event::Sent { conn, len } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    let len = len.min(c.send.len());
                    c.send.drain(..len);
                }
            }
            Event::Failed { conn, error: _ } => {
                if let Some(c) = self.conns.remove(&conn) {
                    actions.push(Action::Log(Log::Reset { conn, peer: c.peer }));
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
        let capacity = self.recv_capacity();
        self.conns.insert(conn, Conn::new(peer, capacity));
        actions.push(Action::SetInterest {
            conn,
            interest: Interest::Read,
        });
    }

    fn received(&mut self, conn: ConnId, len: usize, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let capacity = c.recv.len();
        match c.state {
            ConnState::ReadingHead { buffered } => {
                // A length beyond the spare space is a driver bug: clamp.
                let added = len.min(capacity.saturating_sub(buffered));
                let buffered = buffered.saturating_add(added);
                c.state = ConnState::ReadingHead { buffered };
                if added > 0 && buffered == capacity {
                    actions.push(Action::SetInterest {
                        conn,
                        interest: Interest::None,
                    });
                }
            }
        }
    }

    /// Writable spare space for `conn`'s next read. Empty when the connection
    /// is unknown, not reading, or its buffer is full.
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

    /// The earliest instant at which the core wants [`Event::TimerFired`],
    /// or `None` if it has no deadline. Always `None` for now.
    pub fn next_deadline(&self) -> Option<Instant> {
        None
    }
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
    fn oversized_received_is_clamped() {
        let mut core = core();
        let mut actions = Vec::new();
        let conn = ConnId(1);
        let now = Instant::from_nanos(0);
        core.handle(now, Event::Accepted { conn, peer: PEER }, &mut actions);
        assert_eq!(core.recv_buffer(conn).len(), 10);
        actions.clear();
        core.handle(now, Event::Received { conn, len: 1000 }, &mut actions);
        assert_eq!(
            actions,
            [Action::SetInterest {
                conn,
                interest: Interest::None
            }]
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
