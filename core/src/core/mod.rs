//! The sans-IO core: [`Core`], its buffers API and event dispatch.
//!
//! The driver contract (how a driver talks to the core: events, buffers,
//! readiness, files, upstreams, deadlines, connection limits) is the
//! module documentation of `contract.rs`, next to its vocabulary
//! ([`Event`], [`Action`], [`Interest`], [`ConnId`], [`ConnError`]).

mod contract;
mod limits;
mod proxy;
mod relay;
mod request;
mod static_files;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod timers;
mod upload;

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use crate::config::{Config, Limits, RouteAction};
use crate::conn::{Conn, ConnState, LINGER_SCRATCH, SEND_CAPACITY};
#[cfg(doc)]
use crate::file::RootId;
use crate::file::{Chunk, FileSegment, FileTable};
use crate::http::request::HeadLimits;
use crate::log::Log;
pub use crate::request_body::BodyPending;
use crate::routing::Router;
use crate::time::Instant;
#[cfg(doc)]
use crate::upstream::BackendId;
use crate::upstream::{UpstreamId, UpstreamTable, head_capacity, recv_capacity};

pub use contract::{Action, ConnError, ConnId, Event, Interest};

/// The I/O-free server core. See `contract.rs` for the driver contract.
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
    /// Distinct static roots in config order; a `RootId` indexes it.
    static_roots: Vec<PathBuf>,
    files: FileTable,
    /// Distinct backends in config order; a `BackendId` indexes it.
    backends: Vec<SocketAddr>,
    upstreams: UpstreamTable,
    /// Capacity of each exchange's upstream receive buffer.
    upstream_recv_capacity: usize,
    /// Capacity of each exchange's head buffer.
    head_capacity: usize,
}

/// Capacity of a connection's receive buffer: room for the largest allowed
/// head, and for `max_request_line + 2` bytes so the parser can always
/// reach a verdict on an over-long request line. The single source for the
/// buffer and for the config's memory budget.
pub(crate) fn client_recv_capacity(limits: &Limits) -> usize {
    limits
        .max_head
        .max(limits.max_request_line.saturating_add(2))
}

impl Core {
    /// A core serving `config`, with no connections.
    pub fn new(config: Config) -> Core {
        let mut static_roots: Vec<PathBuf> = Vec::new();
        let mut backends: Vec<SocketAddr> = Vec::new();
        for route in config.hosts.iter().flat_map(|h| &h.routes) {
            match &route.action {
                RouteAction::Static { root, .. } => {
                    if !static_roots.contains(root) {
                        static_roots.push(root.clone());
                    }
                }
                RouteAction::Proxy(proxy) => {
                    if !backends.contains(&proxy.backend) {
                        backends.push(proxy.backend);
                    }
                }
            }
        }
        let limits = &config.limits;
        let upstream_recv_capacity = recv_capacity(limits.max_head);
        let head_capacity = head_capacity(limits.max_head, limits.max_headers);
        Core {
            router: Router::new(&config.hosts),
            head_limits: HeadLimits::from(&config.limits),
            config,
            conns: BTreeMap::new(),
            counted: 0,
            counted_per_ip: BTreeMap::new(),
            refused: 0,
            scratch: vec![0; LINGER_SCRATCH].into_boxed_slice(),
            static_roots,
            files: FileTable::new(),
            backends,
            upstreams: UpstreamTable::new(),
            upstream_recv_capacity,
            head_capacity,
        }
    }

    /// Every distinct backend address (an IP literal and port) in config
    /// order (identical addresses once). A [`BackendId`] is an index into
    /// it; the driver connects to it as it is, resolving nothing.
    pub fn backends(&self) -> &[SocketAddr] {
        &self.backends
    }

    /// Every distinct static root in config order (identical paths once).
    /// A [`RootId`] is an index into it; the driver opens each root once at
    /// startup.
    pub fn static_roots(&self) -> &[PathBuf] {
        &self.static_roots
    }

    /// Capacity of each connection's receive buffer ([`client_recv_capacity`]).
    fn new_recv_capacity(&self) -> usize {
        client_recv_capacity(&self.config.limits)
    }

    /// Process one event at time `now`, appending the resulting actions.
    pub fn handle(&mut self, now: Instant, event: Event, actions: &mut Vec<Action>) {
        match event {
            Event::Accepted { conn, peer } => self.accepted(now, conn, peer, actions),
            Event::Received { conn, len } => self.received(now, conn, len, actions),
            Event::ReadClosed { conn } => self.read_closed(conn, actions),
            Event::Sent { conn, len } => self.sent(now, conn, len, actions),
            Event::Failed { conn, error: _ } => {
                // Log first (peek at the state), then forget, which closes
                // an open file: the order is Log, CloseFile, Close.
                let Some(c) = self.conns.get(&conn) else {
                    return;
                };
                match c.state {
                    // A reset while discarding input after our response is
                    // the expected end of a lingering close, not news.
                    ConnState::Lingering { .. } => {}
                    ConnState::ReadingHead { .. }
                    | ConnState::Idle { .. }
                    | ConnState::Responding { .. }
                    | ConnState::OpeningFile { .. }
                    | ConnState::SendingFile { .. }
                    | ConnState::Connecting { .. }
                    | ConnState::AwaitingResponse { .. }
                    | ConnState::Relaying { .. } => {
                        actions.push(Action::Log(Log::Reset { conn, peer: c.peer }));
                    }
                }
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
            Event::TimerFired => self.timer_fired(now, actions),
            Event::FileOpened { file, meta } => self.file_opened(now, file, meta, actions),
            Event::FileFailed { file, error } => self.file_failed(now, file, error, actions),
            Event::ChunkReady { file, stamp } => self.chunk_ready(now, file, stamp, actions),
            Event::FileSent { conn, len } => self.file_sent(now, conn, len, actions),
            Event::UpstreamConnected { up } => self.upstream_connected(now, up, actions),
            Event::UpstreamFailed { up, error: _ } => self.upstream_failed(now, up, actions),
            Event::UpstreamReceived { up, len } => self.upstream_received(now, up, len, actions),
            Event::UpstreamReadClosed { up } => self.upstream_read_closed(now, up, actions),
            Event::UpstreamSent { up, len } => self.upstream_sent(now, up, len, actions),
        }
    }

    /// Tell the driver if the interest the connection (or its upstream)
    /// needs has changed.
    fn sync_interest(&mut self, conn: ConnId, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // Every render into the send buffer is bounded (fixed responses and
        // file heads by construction, see `every_rendered_head_fits_the_send_buffer`;
        // redirects by an explicit check), and this runs after every one.
        debug_assert!(
            c.send.len() <= SEND_CAPACITY,
            "send buffer holds {} bytes, beyond its capacity",
            c.send.len()
        );
        if let Some((up, interest)) = c.state.upstream_interest() {
            if self.upstreams.set_interest(up, interest) {
                actions.push(Action::SetUpstreamInterest { up, interest });
            }
        }
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
                | ConnState::Responding { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Connecting { .. }
                | ConnState::AwaitingResponse { .. }
                | ConnState::Relaying { .. } => c.recv_spare(),
            },
            None => &mut [],
        }
    }

    /// Bytes waiting to be written to `conn`. Empty when there are none or
    /// the connection is unknown. While a proxied response is relayed they
    /// are any bytes of the connection's own send buffer first (a `100
    /// Continue` of the core's own not yet fully written), then the
    /// response's rewritten head, then its body bytes straight out of the
    /// upstream's receive buffer.
    pub fn send_buffer(&self, conn: ConnId) -> &[u8] {
        match self.conns.get(&conn) {
            Some(c) => match &c.state {
                ConnState::Relaying { body, buffers, .. } => {
                    if !c.send.is_empty() {
                        &c.send
                    } else if buffers.head.is_empty() {
                        body.unsent(&buffers.recv)
                    } else {
                        &buffers.head
                    }
                }
                ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::Responding { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Connecting { .. }
                | ConnState::AwaitingResponse { .. }
                | ConnState::Lingering { .. } => &c.send,
            },
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
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. } => c.recv.len(),
        })
    }

    /// The file range the driver may `sendfile` to `conn` now: only once
    /// [`Core::send_buffer`] is empty and the chunk's `ChunkReady` has
    /// arrived. `None` when there is none or the connection is unknown.
    pub fn send_file(&self, conn: ConnId) -> Option<FileSegment> {
        let c = self.conns.get(&conn)?;
        if !c.send.is_empty() {
            return None;
        }
        let ConnState::SendingFile {
            file,
            offset,
            chunk,
            ..
        } = c.state
        else {
            return None;
        };
        match chunk {
            Chunk::Ready { end, .. } if end > offset => Some(FileSegment {
                file,
                offset,
                len: end.saturating_sub(offset),
            }),
            Chunk::Ready { .. } | Chunk::Reading { .. } | Chunk::Done => None,
        }
    }

    /// The most [`Core::send_buffer`] can currently hand out for `conn`:
    /// its own send buffer's capacity, or while a proxied response is
    /// relayed, the capacity of the exchange buffer it writes from (the
    /// head buffer, then the upstream's receive buffer); 0 for unknown
    /// connections. It can change with the connection's state. For
    /// drivers' invariant checks (the simulation), not for I/O.
    #[doc(hidden)]
    pub fn send_capacity(&self, conn: ConnId) -> usize {
        self.conns.get(&conn).map_or(0, |c| match &c.state {
            ConnState::Relaying { buffers, .. } => {
                if !c.send.is_empty() {
                    SEND_CAPACITY
                } else if buffers.head.is_empty() {
                    buffers.recv.len()
                } else {
                    self.head_capacity
                }
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Lingering { .. } => SEND_CAPACITY,
        })
    }

    /// Writable space for upstream `up`'s next read. Empty when the
    /// upstream is unknown, not reading, or (in a body) still holds body
    /// bytes not yet sent to the client; a body read is always offered the
    /// whole buffer.
    ///
    /// If the driver writes any bytes into it, it reports them with
    /// [`Event::UpstreamReceived`] for `up` as the very next `handle` call.
    pub fn upstream_recv_buffer(&mut self, up: UpstreamId) -> &mut [u8] {
        let Some(conn) = self.upstreams.conn(up) else {
            return &mut [];
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return &mut [];
        };
        if !c.state.reads_upstream(up) {
            return &mut [];
        }
        match &mut c.state {
            ConnState::AwaitingResponse {
                buffers, filled, ..
            } => buffers.recv.get_mut(*filled..).unwrap_or_default(),
            ConnState::Relaying { body, buffers, .. } => body.spare(&mut buffers.recv),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Lingering { .. } => &mut [],
        }
    }

    /// Bytes waiting to be written to upstream `up`: the rewritten request
    /// head, then the request body's bytes straight out of the client's
    /// receive buffer. Empty when there are none or the upstream is
    /// unknown.
    pub fn upstream_send_buffer(&self, up: UpstreamId) -> &[u8] {
        let Some(c) = self
            .upstreams
            .conn(up)
            .and_then(|conn| self.conns.get(&conn))
        else {
            return &[];
        };
        match &c.state {
            ConnState::AwaitingResponse {
                up: own,
                buffers,
                request,
                ..
            } if *own == up => {
                if buffers.head.is_empty() {
                    request.body.unsent(&c.recv)
                } else {
                    &buffers.head
                }
            }
            ConnState::AwaitingResponse { .. }
            | ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => &[],
        }
    }

    /// Capacity of upstream `up`'s receive buffer; 0 for unknown or closed
    /// upstreams. For drivers' invariant checks (the simulation), not for
    /// I/O.
    #[doc(hidden)]
    pub fn upstream_recv_capacity(&self, up: UpstreamId) -> usize {
        self.upstreams
            .conn(up)
            .map_or(0, |_| self.upstream_recv_capacity)
    }

    /// Capacity of upstream `up`'s send buffer; 0 for unknown or closed
    /// upstreams. For drivers' invariant checks, not for I/O.
    #[doc(hidden)]
    pub fn upstream_send_capacity(&self, up: UpstreamId) -> usize {
        self.upstreams.conn(up).map_or(0, |_| self.head_capacity)
    }

    /// While upstream `up` is relaying a body: how many body bytes of its
    /// receive buffer are not yet sent to the client, and how long a
    /// chunked-framing tail it keeps behind them (moved to the front once
    /// they are sent). `None` outside a body. For drivers' invariant checks
    /// (fill, drain, refill), not for I/O.
    #[doc(hidden)]
    pub fn upstream_body_pending(&self, up: UpstreamId) -> Option<BodyPending> {
        let c = self
            .upstreams
            .conn(up)
            .and_then(|conn| self.conns.get(&conn))?;
        match &c.state {
            ConnState::Relaying { body, .. } if body.upstream() == Some(up) => body.pending(),
            ConnState::Relaying { .. }
            | ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Lingering { .. } => None,
        }
    }

    /// While `conn` forwards a request body: how many body bytes of its
    /// receive buffer are not yet written to the backend, and how long a
    /// chunked-framing tail it keeps at the front. `None` outside a body
    /// (or once it is fully written). For drivers' invariant checks (fill,
    /// drain, refill), not for I/O.
    #[doc(hidden)]
    pub fn request_body_pending(&self, conn: ConnId) -> Option<BodyPending> {
        match &self.conns.get(&conn)?.state {
            ConnState::Connecting { request, .. } | ConnState::AwaitingResponse { request, .. } => {
                request.body.pending()
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => None,
        }
    }

    /// The earliest instant at which the core wants [`Event::TimerFired`],
    /// or `None` if it has no connections (every connection has a deadline).
    pub fn next_deadline(&self) -> Option<Instant> {
        self.conns.values().map(|c| c.state.deadline()).min()
    }
}
