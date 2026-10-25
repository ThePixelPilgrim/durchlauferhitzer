//! The sans-IO core and its contract with drivers.
//!
//! # Contract
//!
//! The core never touches sockets, clocks or files. A *driver* (the
//! simulation in tests, the epoll driver in production) owns all I/O and talks
//! to the core through exactly four channels:
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
//! 4. **Files.** File bytes never pass through the core. The core asks the
//!    driver to open a file ([`Action::OpenFile`]), to read a chunk of it
//!    into the page cache ([`Action::Readahead`]) and to close it
//!    ([`Action::CloseFile`]); the driver answers with [`Event::FileOpened`],
//!    [`Event::ChunkReady`] or [`Event::FileFailed`]. Once a chunk is ready
//!    the driver `sendfile`s the range [`Core::send_file`] returns and
//!    reports how many bytes the socket accepted with [`Event::FileSent`].
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
//! Rules for files:
//!
//! - The core allocates [`FileId`]s, sequentially from 1, and never reuses
//!   one. A [`RootId`] indexes [`Core::static_roots`]: the driver opens each
//!   root once at startup.
//! - `OpenFile { root, path }` names a path as relative segments (decoded
//!   bytes, never `.`, `..`, empty, or containing `/` or NUL; no segments
//!   means the root itself). The driver resolves it beneath the root,
//!   without following symlinks anywhere in the path and without ever
//!   leaving the root (`openat2` with `RESOLVE_BENEATH |
//!   RESOLVE_NO_SYMLINKS`), and reports the metadata of the open descriptor.
//! - Every `OpenFile` is answered by exactly one `FileOpened` or
//!   `FileFailed`, and every `Readahead` by exactly one `ChunkReady` or
//!   `FileFailed`, even after the connection that asked closed. The core
//!   then answers a late `FileOpened` with `CloseFile` and ignores a late
//!   `FileFailed`. `ChunkReady` carries the file's stamp at the time the
//!   chunk was read.
//! - A file is open from its `FileOpened` until the core's `CloseFile`; a
//!   failed open leaves nothing to close. `CloseFile` comes exactly once per
//!   opened file, also when its connection closes, and cancels any
//!   `Readahead` in flight: after `CloseFile` the driver delivers no more
//!   events for that file (the core ignores any that still arrive).
//! - The driver writes `send_buffer` before `send_file`: `send_file` returns
//!   a segment only while the send buffer is empty and that chunk's
//!   `ChunkReady` has arrived. Interest is [`Interest::Write`] while either
//!   has bytes, and [`Interest::None`] while the connection waits on the
//!   disk.
//! - `FileSent { len }` reports how many bytes of the segment the socket
//!   accepted (more than the segment is clamped). A `sendfile` that hits end
//!   of file before the segment ends (the file shrank) is reported as
//!   `FileFailed { error: Truncated }`, after a `FileSent` for any bytes it
//!   did send.
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
//! - waiting on the disk (open or readahead in flight): the same
//!   `send_idle_timeout`, also reset whenever the disk completes; on expiry
//!   the connection is closed and its file, if open, closed with it;
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
//! connections or files are ignored, a duplicate `Accepted` is ignored, a
//! file event that does not fit the file's state is ignored, and a
//! `Received`/`Sent`/`FileSent` length larger than the space or segment
//! offered is clamped.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use crate::config::{Config, Limits, Route, RouteAction};
use crate::conn::{
    Conn, ConnState, Counted, LINGER_SCRATCH, LINGER_TIMEOUT, SEND_CAPACITY, UnreadBody,
};
use crate::file::{
    CHUNK_SIZE, Chunk, FileError, FileId, FileKind, FileMeta, FileSegment, FileStamp, FileTable,
    Owner, RootId,
};
use crate::http::mime::content_type;
use crate::http::request::{
    BodyFraming, HeadLimits, HeadParser, HttpError, Method, ParseStatus, Persistence, RequestHead,
};
use crate::http::response::{BodyMode, render_file_head, render_fixed};
use crate::http::static_path::{TrailingSlash, relative_segments, trailing_slash};
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
    /// The `OpenFile` for `file` succeeded; `meta` describes the open
    /// descriptor.
    FileOpened { file: FileId, meta: FileMeta },
    /// An `OpenFile`, `Readahead` or `sendfile` for `file` failed.
    FileFailed { file: FileId, error: FileError },
    /// The last `Readahead` for `file` completed; `stamp` is the file's
    /// stamp when the chunk was read.
    ChunkReady { file: FileId, stamp: FileStamp },
    /// The socket accepted the first `len` bytes of the segment last
    /// returned by [`Core::send_file`] for `conn`.
    FileSent { conn: ConnId, len: u64 },
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
    /// Write from [`Core::send_buffer`], then from [`Core::send_file`],
    /// when writable.
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
    /// Open `path` (relative segments) beneath static root `root`, without
    /// following symlinks and without leaving the root, as `file`.
    OpenFile {
        file: FileId,
        root: RootId,
        path: Vec<Vec<u8>>,
    },
    /// Read `len` bytes of `file` from `offset` into the page cache, then
    /// report `ChunkReady`. Never beyond the size reported at open.
    Readahead { file: FileId, offset: u64, len: u64 },
    /// Close `file`; drop any completion still pending for it.
    CloseFile { file: FileId },
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
    /// Distinct static roots in config order; a `RootId` indexes it.
    static_roots: Vec<PathBuf>,
    files: FileTable,
}

impl Core {
    /// A core serving `config`, with no connections.
    pub fn new(config: Config) -> Core {
        let mut static_roots: Vec<PathBuf> = Vec::new();
        for route in config.hosts.iter().flat_map(|h| &h.routes) {
            match &route.action {
                RouteAction::Static { root } => {
                    if !static_roots.contains(root) {
                        static_roots.push(root.clone());
                    }
                }
                RouteAction::Proxy(_) => {}
            }
        }
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
        }
    }

    /// Every distinct static root in config order (identical paths once).
    /// A [`RootId`] is an index into it; the driver opens each root once at
    /// startup.
    pub fn static_roots(&self) -> &[PathBuf] {
        &self.static_roots
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
                if self.forget(conn, actions).is_some() {
                    actions.push(Action::Close { conn });
                }
            }
            Event::Sent { conn, len } => self.sent(now, conn, len, actions),
            Event::Failed { conn, error: _ } => {
                if let Some(c) = self.forget(conn, actions) {
                    match c.state {
                        // A reset while discarding input after our response is
                        // the expected end of a lingering close, not news.
                        ConnState::Lingering { .. } => {}
                        ConnState::ReadingHead { .. }
                        | ConnState::Idle { .. }
                        | ConnState::Responding { .. }
                        | ConnState::OpeningFile { .. }
                        | ConnState::SendingFile { .. } => {
                            actions.push(Action::Log(Log::Reset { conn, peer: c.peer }));
                        }
                    }
                    actions.push(Action::Close { conn });
                }
            }
            Event::TimerFired => self.timer_fired(now, actions),
            Event::FileOpened { file, meta } => self.file_opened(now, file, meta, actions),
            Event::FileFailed { file, error: _ } => self.file_failed(file, actions),
            Event::ChunkReady { file, stamp } => self.chunk_ready(now, file, stamp, actions),
            Event::FileSent { conn, len } => self.file_sent(now, conn, len, actions),
        }
    }

    /// Remove `conn` from the map and release its share of the limits and
    /// its file: an open file is closed, an open in flight is orphaned.
    fn forget(&mut self, conn: ConnId, actions: &mut Vec<Action>) -> Option<Conn> {
        let c = self.conns.remove(&conn)?;
        match c.state {
            ConnState::OpeningFile { file, .. } => self.files.orphan(file),
            ConnState::SendingFile { file, .. } => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::Lingering { .. } => {}
        }
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
            ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. } => return,
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
        // Progress resets the send idle deadline.
        if len > 0 {
            c.state
                .set_deadline(now.saturating_add(limits.send_idle_timeout));
        }
        if c.send.is_empty() {
            match c.state {
                ConnState::Responding {
                    persistence,
                    unread_body,
                    buffered,
                    ..
                } => {
                    c.state = finish(
                        conn,
                        c,
                        persistence,
                        unread_body,
                        buffered,
                        now,
                        limits,
                        actions,
                    );
                }
                ConnState::SendingFile {
                    file,
                    chunk: Chunk::Done,
                    persistence,
                    buffered,
                    ..
                } => {
                    self.files.release(file);
                    actions.push(Action::CloseFile { file });
                    c.state = finish(
                        conn,
                        c,
                        persistence,
                        UnreadBody::None,
                        buffered,
                        now,
                        limits,
                        actions,
                    );
                }
                ConnState::SendingFile { .. }
                | ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::Lingering { .. } => {}
            }
        }
        self.try_parse(now, conn, actions);
        self.sync_interest(conn, actions);
    }

    /// The driver opened `file`: answer the request it belongs to.
    fn file_opened(
        &mut self,
        now: Instant,
        file: FileId,
        meta: FileMeta,
        actions: &mut Vec<Action>,
    ) {
        let conn = match self.files.owner(file) {
            Owner::Conn(conn) => conn,
            Owner::Orphan => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
                return;
            }
            Owner::Unknown => return,
        };
        let deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::OpeningFile {
            file: opening,
            content_type,
            slash,
            persistence,
            buffered,
            ..
        } = c.state
        else {
            return;
        };
        if opening != file {
            return;
        }
        let size = meta.stamp.size;
        let status = match (meta.kind, slash) {
            (FileKind::Regular, TrailingSlash::Absent) => Status::Ok,
            // Not served yet in this stage.
            (FileKind::Regular, TrailingSlash::Present)
            | (FileKind::Directory | FileKind::Other, _) => Status::NotImplemented,
        };
        actions.push(Action::Log(Log::Response {
            conn,
            peer: c.peer,
            status,
            request: c.request.take(),
        }));
        match (meta.kind, slash) {
            (FileKind::Regular, TrailingSlash::Absent) => {
                render_file_head(content_type, size, persistence, &mut c.send);
                let chunk = if size == 0 {
                    Chunk::Done
                } else {
                    let end = size.min(CHUNK_SIZE);
                    actions.push(Action::Readahead {
                        file,
                        offset: 0,
                        len: end,
                    });
                    Chunk::Reading { end }
                };
                c.state = ConnState::SendingFile {
                    file,
                    size,
                    offset: 0,
                    chunk,
                    persistence,
                    buffered,
                    deadline,
                };
            }
            (FileKind::Regular, TrailingSlash::Present)
            | (FileKind::Directory | FileKind::Other, _) => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
                render_fixed(
                    Status::NotImplemented,
                    Persistence::Close,
                    BodyMode::Full,
                    &mut c.send,
                );
                c.state = ConnState::Responding {
                    persistence: Persistence::Close,
                    unread_body: UnreadBody::None,
                    buffered,
                    deadline,
                };
            }
        }
        self.sync_interest(conn, actions);
    }

    /// An open, readahead or sendfile of `file` failed: close its
    /// connection (and the file, if it was open).
    fn file_failed(&mut self, file: FileId, actions: &mut Vec<Action>) {
        let conn = match self.files.owner(file) {
            Owner::Conn(conn) => conn,
            Owner::Orphan => {
                self.files.release(file);
                return;
            }
            Owner::Unknown => return,
        };
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        if c.state.file() != Some(file) {
            return;
        }
        if let Some(c) = self.forget(conn, actions) {
            // A failed open leaves nothing open to close.
            if let ConnState::OpeningFile { .. } = c.state {
                self.files.release(file);
            }
            actions.push(Action::Close { conn });
        }
    }

    /// The readahead in flight for `file` completed.
    fn chunk_ready(
        &mut self,
        now: Instant,
        file: FileId,
        stamp: FileStamp,
        actions: &mut Vec<Action>,
    ) {
        let Owner::Conn(conn) = self.files.owner(file) else {
            return;
        };
        let deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::SendingFile {
            file: sending,
            size,
            offset,
            chunk: Chunk::Reading { end },
            persistence,
            buffered,
            ..
        } = c.state
        else {
            return;
        };
        if sending != file {
            return;
        }
        c.state = ConnState::SendingFile {
            file,
            size,
            offset,
            chunk: Chunk::Ready { end, stamp },
            persistence,
            buffered,
            deadline,
        };
        self.sync_interest(conn, actions);
    }

    /// The socket accepted `len` bytes of `conn`'s file segment.
    fn file_sent(&mut self, now: Instant, conn: ConnId, len: u64, actions: &mut Vec<Action>) {
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::SendingFile {
            file,
            size,
            offset,
            chunk: Chunk::Ready { end, stamp },
            persistence,
            buffered,
            ..
        } = c.state
        else {
            return;
        };
        // No segment was offered while the send buffer held bytes.
        if !c.send.is_empty() {
            return;
        }
        let len = len.min(end.saturating_sub(offset));
        if len == 0 {
            return;
        }
        let offset = offset.saturating_add(len);
        let deadline = now.saturating_add(limits.send_idle_timeout);
        let chunk = if offset < end {
            Chunk::Ready { end, stamp }
        } else if end < size {
            let next = size.min(end.saturating_add(CHUNK_SIZE));
            actions.push(Action::Readahead {
                file,
                offset: end,
                len: next.saturating_sub(end),
            });
            Chunk::Reading { end: next }
        } else {
            Chunk::Done
        };
        c.state = match chunk {
            Chunk::Done => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
                finish(
                    conn,
                    c,
                    persistence,
                    UnreadBody::None,
                    buffered,
                    now,
                    limits,
                    actions,
                )
            }
            Chunk::Ready { .. } | Chunk::Reading { .. } => ConnState::SendingFile {
                file,
                size,
                offset,
                chunk,
                persistence,
                buffered,
                deadline,
            },
        };
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
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
            // The client stopped reading, or the disk did not answer in
            // time: close without further writes.
            ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. } => {
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(TimeoutKind::Send),
                }));
                self.forget(conn, actions);
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
                    &self.static_roots,
                    &mut self.files,
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
                | ConnState::Responding { .. }
                | ConnState::OpeningFile { .. }
                | ConnState::SendingFile { .. } => c.recv_spare(),
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
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. } => c.recv.len(),
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

/// The response on `conn` is fully written: keep the connection for the
/// next request, or shut down its sending side and linger.
#[allow(
    clippy::too_many_arguments,
    reason = "free function over disjoint borrows of the core's fields"
)]
fn finish(
    conn: ConnId,
    c: &mut Conn,
    persistence: Persistence,
    unread_body: UnreadBody,
    buffered: usize,
    now: Instant,
    limits: &Limits,
    actions: &mut Vec<Action>,
) -> ConnState {
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
}

/// Where a body-less GET on a static route is served from: the root's id
/// and the path's relative segments. `None` for anything this stage does
/// not serve from a file.
fn static_target(
    route: &Route,
    head: &RequestHead,
    unread_body: UnreadBody,
    static_roots: &[PathBuf],
) -> Option<(RootId, Vec<Vec<u8>>)> {
    let root = match &route.action {
        RouteAction::Static { root } => root,
        RouteAction::Proxy(_) => return None,
    };
    match (&head.method, unread_body) {
        (Method::Get, UnreadBody::None) => {}
        (Method::Get, UnreadBody::Pending) | (Method::Head | Method::Other(_), _) => return None,
    }
    let index = static_roots.iter().position(|r| r == root)?;
    let root = RootId(u32::try_from(index).ok()?);
    let path = relative_segments(&head.target.path, route.segments.len()).ok()?;
    Some((root, path))
}

/// Answer a complete request head on `c`, which then responds until
/// `send_deadline` (unless the socket makes progress). `leftover` bytes at
/// the front of `c.recv` wait for the next request.
#[allow(
    clippy::too_many_arguments,
    reason = "free function over disjoint borrows of the core's fields"
)]
fn respond(
    c: &mut Conn,
    router: &Router,
    static_roots: &[PathBuf],
    files: &mut FileTable,
    conn: ConnId,
    head: RequestHead,
    leftover: usize,
    send_deadline: Instant,
    actions: &mut Vec<Action>,
) {
    let route = router.route(head.host.as_deref(), &head.target.path);
    let unread_body = match head.body {
        BodyFraming::None => UnreadBody::None,
        BodyFraming::Length(_) | BodyFraming::Chunked => UnreadBody::Pending,
    };
    let target = route
        .ok()
        .and_then(|route| static_target(route, &head, unread_body, static_roots));
    if let Some((root, path)) = target {
        let name = path.last().map(Vec::as_slice).unwrap_or_default();
        let content_type = content_type(name);
        let slash = trailing_slash(&head.target.path);
        let file = files.allocate(conn);
        actions.push(Action::OpenFile { file, root, path });
        c.state = ConnState::OpeningFile {
            file,
            content_type,
            slash,
            persistence: head.persistence,
            buffered: leftover,
            deadline: send_deadline,
        };
        c.request = Some(RequestSummary {
            method: head.method,
            host: head.host,
            path: head.target.path,
        });
        return;
    }
    // Everything else on a matched route is not implemented yet in this
    // stage: answered 501 and the connection closed.
    let (status, persistence) = match route {
        Ok(_) => (Status::NotImplemented, Persistence::Close),
        Err(_) => (Status::NotFound, head.persistence),
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
    use crate::file::{FileError, FileKind, FileMeta, FileStamp};
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
        // The complete head was answered: `/` is opened as a file.
        assert!(
            actions.iter().any(|a| matches!(a, Action::OpenFile { .. })),
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

    const FILES: &str = "listen 127.0.0.1:8080; \
        host a.test { path / { static /srv/a; } path /x { static /srv/b; } } \
        host b.test { path / { static /srv/a; } path /p { proxy 127.0.0.1:3000; \
        max_request_body 1MiB; max_response_body unlimited; } path /c { static /srv/c; } }";

    fn files_core() -> Core {
        Core::new(crate::config::load(FILES).unwrap())
    }

    /// Accept `id` and deliver `request` in one read; returns the actions.
    fn request(core: &mut Core, now: Instant, id: u64, request: &[u8]) -> Vec<Action> {
        let mut actions = accept(core, now, id, PEER);
        let conn = ConnId(id);
        let spare = core.recv_buffer(conn);
        spare
            .get_mut(..request.len())
            .unwrap()
            .copy_from_slice(request);
        let len = request.len();
        actions.clear();
        core.handle(now, Event::Received { conn, len }, &mut actions);
        actions
    }

    const GET_F: &[u8] = b"GET /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n";

    fn stamp(size: u64) -> FileStamp {
        FileStamp {
            size,
            modified_ns: 7,
        }
    }

    fn regular(size: u64) -> FileMeta {
        FileMeta {
            kind: FileKind::Regular,
            stamp: stamp(size),
        }
    }

    #[test]
    fn static_roots_are_distinct_in_config_order() {
        let core = files_core();
        assert_eq!(
            core.static_roots(),
            [
                std::path::PathBuf::from("/srv/a"),
                std::path::PathBuf::from("/srv/b"),
                std::path::PathBuf::from("/srv/c"),
            ]
        );
    }

    #[test]
    fn file_is_opened_read_sent_and_closed() {
        let mut core = files_core();
        let now = secs(0);
        let conn = ConnId(1);
        let file = FileId(1);
        let actions = request(&mut core, now, 1, GET_F);
        assert_eq!(
            actions,
            [
                Action::OpenFile {
                    file,
                    root: RootId(1),
                    path: vec![b"f.txt".to_vec()],
                },
                Action::SetInterest {
                    conn,
                    interest: Interest::None
                },
            ]
        );
        assert_eq!(core.send_file(conn), None);
        let mut actions = Vec::new();
        let meta = regular(3);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::Log(Log::Response {
                    conn,
                    peer: PEER,
                    status: Status::Ok,
                    request: Some(RequestSummary {
                        method: Method::Get,
                        host: Some("a.test".to_owned()),
                        path: "/x/f.txt".to_owned(),
                    }),
                }),
                Action::Readahead {
                    file,
                    offset: 0,
                    len: 3
                },
                Action::SetInterest {
                    conn,
                    interest: Interest::Write
                },
            ]
        );
        let mut head = Vec::new();
        crate::http::response::render_file_head(
            "text/plain; charset=utf-8",
            3,
            Persistence::KeepAlive,
            &mut head,
        );
        assert_eq!(core.send_buffer(conn), head.as_slice());
        // Ready before the head is written: still nothing to sendfile.
        actions.clear();
        let stamp = stamp(3);
        core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);
        assert_eq!(core.send_file(conn), None);
        let len = head.len();
        core.handle(now, Event::Sent { conn, len }, &mut actions);
        assert_eq!(
            core.send_file(conn),
            Some(FileSegment {
                file,
                offset: 0,
                len: 3
            })
        );
        assert_eq!(core.next_deadline(), Some(secs(30)));
        actions.clear();
        core.handle(secs(1), Event::FileSent { conn, len: 2 }, &mut actions);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(
            core.send_file(conn),
            Some(FileSegment {
                file,
                offset: 2,
                len: 1
            })
        );
        assert_eq!(core.next_deadline(), Some(secs(31)));
        core.handle(secs(1), Event::FileSent { conn, len: 9 }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::CloseFile { file },
                Action::SetInterest {
                    conn,
                    interest: Interest::Read
                },
            ]
        );
        assert_eq!(core.send_file(conn), None);
        // The next request gets a fresh file id.
        let next = GET_F;
        let spare = core.recv_buffer(conn);
        spare.get_mut(..next.len()).unwrap().copy_from_slice(next);
        actions.clear();
        let len = next.len();
        core.handle(secs(1), Event::Received { conn, len }, &mut actions);
        assert!(
            actions.contains(&Action::OpenFile {
                file: FileId(2),
                root: RootId(1),
                path: vec![b"f.txt".to_vec()],
            }),
            "{actions:?}"
        );
    }

    #[test]
    fn empty_file_sends_only_the_head() {
        let mut core = files_core();
        let now = secs(0);
        let conn = ConnId(1);
        let file = FileId(1);
        request(&mut core, now, 1, GET_F);
        let mut actions = Vec::new();
        let meta = regular(0);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::Readahead { .. })),
            "{actions:?}"
        );
        actions.clear();
        let len = core.send_buffer(conn).len();
        core.handle(now, Event::Sent { conn, len }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::CloseFile { file },
                Action::SetInterest {
                    conn,
                    interest: Interest::Read
                },
            ]
        );
    }

    #[test]
    fn directory_is_closed_and_answered_501() {
        let mut core = files_core();
        let now = secs(0);
        let conn = ConnId(1);
        let file = FileId(1);
        request(&mut core, now, 1, GET_F);
        let mut actions = Vec::new();
        let meta = FileMeta {
            kind: FileKind::Directory,
            stamp: stamp(0),
        };
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        assert!(actions.contains(&Action::CloseFile { file }), "{actions:?}");
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::Log(Log::Response {
                    status: Status::NotImplemented,
                    ..
                })
            )),
            "{actions:?}"
        );
        let mut expected = Vec::new();
        render_fixed(
            Status::NotImplemented,
            Persistence::Close,
            BodyMode::Full,
            &mut expected,
        );
        assert_eq!(core.send_buffer(conn), expected.as_slice());
    }

    #[test]
    fn file_failure_closes_the_connection_and_its_file() {
        let mut core = files_core();
        let now = secs(0);
        let file = FileId(1);
        let conn = ConnId(1);
        // Failed open: no file to close.
        request(&mut core, now, 1, GET_F);
        let mut actions = Vec::new();
        let error = FileError::NotFound;
        core.handle(now, Event::FileFailed { file, error }, &mut actions);
        assert_eq!(actions, [Action::Close { conn }]);
        // Failed readahead: the open file is closed too.
        let file = FileId(2);
        let conn = ConnId(2);
        request(&mut core, now, 2, GET_F);
        let meta = regular(3);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        actions.clear();
        let error = FileError::Other;
        core.handle(now, Event::FileFailed { file, error }, &mut actions);
        assert_eq!(
            actions,
            [Action::CloseFile { file }, Action::Close { conn }]
        );
        assert!(core.conns.is_empty());
    }

    #[test]
    fn late_file_events_after_close() {
        let mut core = files_core();
        let now = secs(0);
        let conn = ConnId(1);
        let file = FileId(1);
        request(&mut core, now, 1, GET_F);
        let mut actions = Vec::new();
        // The open is still in flight when the send idle deadline passes.
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert!(actions.contains(&Action::Close { conn }), "{actions:?}");
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::CloseFile { .. })),
            "{actions:?}"
        );
        assert_eq!(core.next_deadline(), None);
        actions.clear();
        let meta = regular(3);
        core.handle(secs(31), Event::FileOpened { file, meta }, &mut actions);
        assert_eq!(actions, [Action::CloseFile { file }]);
        // Anything later for that file, or for files never opened, is ignored.
        actions.clear();
        let stamp = stamp(3);
        for event in [
            Event::FileOpened { file, meta },
            Event::ChunkReady { file, stamp },
            Event::FileFailed {
                file,
                error: FileError::Truncated,
            },
            Event::FileOpened {
                file: FileId(9),
                meta,
            },
            Event::FileSent { conn, len: 1 },
        ] {
            core.handle(secs(31), event, &mut actions);
        }
        assert!(actions.is_empty(), "{actions:?}");

        // A connection reading a file that goes away closes its file.
        let conn = ConnId(2);
        let file = FileId(2);
        request(&mut core, now, 2, GET_F);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        actions.clear();
        core.handle(now, Event::ReadClosed { conn }, &mut actions);
        assert_eq!(
            actions,
            [Action::CloseFile { file }, Action::Close { conn }]
        );
        actions.clear();
        core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(core.files.len(), 0);
    }

    #[test]
    fn only_body_less_get_of_a_file_opens_it() {
        let mut core = files_core();
        let now = secs(0);
        for (id, req) in [
            (
                1,
                b"HEAD /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n".as_slice(),
            ),
            (2, b"POST /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n"),
            (3, b"GET /x/.f HTTP/1.1\r\nHost: a.test\r\n\r\n"),
            (
                4,
                b"GET /x/f HTTP/1.1\r\nHost: a.test\r\nContent-Length: 1\r\n\r\nx",
            ),
            (5, b"GET /p/f HTTP/1.1\r\nHost: b.test\r\n\r\n"),
        ] {
            let actions = request(&mut core, now, id, req);
            assert!(
                !actions.iter().any(|a| matches!(a, Action::OpenFile { .. })),
                "{id}: {actions:?}"
            );
            assert!(
                actions.iter().any(|a| matches!(
                    a,
                    Action::Log(Log::Response {
                        status: Status::NotImplemented,
                        ..
                    })
                )),
                "{id}: {actions:?}"
            );
        }
    }
}
