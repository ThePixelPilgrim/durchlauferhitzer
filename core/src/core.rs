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
//!   block, and once it is empty, from [`Core::send_file`] until the segment
//!   is done or the socket would block. An edge-triggered driver must loop
//!   to `EAGAIN`. `EAGAIN` is not reported (`Sent` and `FileSent` are never
//!   0); a short count only means the socket is full. A `sendfile` that
//!   returns 0 on a writable socket while the segment is unfinished has hit
//!   end of file: the driver reports `FileFailed { error: Truncated }`.
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
//!   `FileFailed`, even after the connection that asked closed: the core
//!   never closes a file whose open is unanswered. It answers a late
//!   `FileOpened` with `CloseFile` and ignores a late `FileFailed`.
//! - A connection holds at most one file id at a time: for a directory
//!   requested with a trailing `/`, the core closes the directory before it
//!   opens its `index.html` under a new id. A HEAD request's file is closed
//!   right after its head is queued, without any `Readahead`.
//! - A file is open from its `FileOpened` until the core's `CloseFile`; a
//!   failed open leaves nothing to close. `CloseFile` comes exactly once per
//!   opened file, also when its connection closes.
//! - While the file is open, every `Readahead` is answered by exactly one
//!   `ChunkReady` or `FileFailed`; `ChunkReady` carries the file's stamp
//!   (of the open descriptor) at the time the chunk was read. `CloseFile`
//!   supersedes a `Readahead` still in flight: after `CloseFile` the driver
//!   delivers no event for that file at all. It keeps the descriptor open
//!   until an in-flight readahead worker has returned and then drops that
//!   completion, so a reused descriptor number can never receive it. (The
//!   core ignores events for closed files anyway.)
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
//! - Every `ChunkReady` stamp is checked against the stamp at open under
//!   the route's `on_file_change`: `abort` (the default) fails the transfer
//!   with `Changed` on any difference; `allow_growth` keeps sending exactly
//!   the size at open when the file only grew, and fails with `Changed` for
//!   the same size with another modification time and with `Truncated` for
//!   a smaller size.
//! - A failure after the response head was queued (any `FileFailed` while
//!   sending, or a stamp that fails the check) aborts the transfer: the core
//!   emits `CloseFile`, logs [`Log::FileAborted`] and closes the connection
//!   at once, with no lingering and no further writes, so the client sees a
//!   cut body, never a complete-looking short one. A failed open is
//!   answered 404; `Truncated` or `Changed` in answer to `OpenFile` is a
//!   driver bug and closes the connection.
//! - A directory requested without its trailing `/` is answered 301 to the
//!   path plus `/` (query kept), unless that response would not fit the
//!   send buffer: then 414. The send buffer never holds more than its
//!   capacity.
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
//!   it is logged as a disk timeout and the connection is closed: an open in
//!   flight becomes an orphaned open, an open file is closed with it. (A
//!   readahead in flight counts as waiting on the disk even while the send
//!   buffer still holds part of the head.)
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
//! - A connection that closes (or times out) while its `OpenFile` is
//!   unanswered leaves an *orphaned open*. It keeps counting toward
//!   both `max_connections` and its peer's `max_connections_per_ip` until
//!   its `FileOpened` (answered with `CloseFile`) or `FileFailed` arrives,
//!   so a hung disk cannot accumulate more open files than connections, and
//!   one peer looping request-then-reset against it is capped at
//!   `max_connections_per_ip` orphans instead of taking every global slot.
//!   (A driver cannot cancel an open blocked in the kernel, so the core
//!   waits for the answer rather than lying about it.)
//!
//! The core tolerates driver mistakes without panicking: events for unknown
//! connections or files are ignored, a duplicate `Accepted` is ignored, a
//! file event that does not fit the file's state is ignored, and a
//! `Received`/`Sent`/`FileSent` length larger than the space or segment
//! offered is clamped.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use crate::config::{Config, Limits, OnFileChange, Route, RouteAction};
use crate::conn::{
    Conn, ConnState, Counted, LINGER_SCRATCH, LINGER_TIMEOUT, OpenStep, SEND_CAPACITY,
    StaticRequest, UnreadBody,
};
use crate::file::{
    Chunk, FILE_CHUNK, FileError, FileId, FileKind, FileMeta, FileSegment, FileStamp, FileTable,
    OrphanSlots, Owner, RootId, check_stamp,
};
use crate::http::mime::content_type;
use crate::http::request::{
    BodyFraming, HeadLimits, HeadParser, HttpError, KnownMethod, Method, ParseStatus, Persistence,
    RequestHead,
};
use crate::http::response::{
    BodyMode, redirect_len, render_file_head, render_fixed, render_redirect,
};
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
                RouteAction::Static { root, .. } => {
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
            Event::FileFailed { file, error } => self.file_failed(now, file, error, actions),
            Event::ChunkReady { file, stamp } => self.chunk_ready(now, file, stamp, actions),
            Event::FileSent { conn, len } => self.file_sent(now, conn, len, actions),
        }
    }

    /// Remove `conn` from the map and release its share of the limits and
    /// its file: an open file is closed, an open in flight is orphaned.
    fn forget(&mut self, conn: ConnId, actions: &mut Vec<Action>) -> Option<Conn> {
        let c = self.conns.remove(&conn)?;
        let peer = c.peer.to_canonical();
        match c.state {
            ConnState::OpeningFile { file, .. } => {
                let slots = match c.counted {
                    Counted::Yes => OrphanSlots::Held { peer },
                    Counted::No => OrphanSlots::None,
                };
                self.files.orphan(file, slots);
            }
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
            Counted::Yes => match c.state {
                // An orphaned open keeps both slots until the disk answers.
                ConnState::OpeningFile { .. } => {}
                ConnState::ReadingHead { .. }
                | ConnState::Idle { .. }
                | ConnState::Responding { .. }
                | ConnState::SendingFile { .. }
                | ConnState::Lingering { .. } => self.release_slots(peer),
            },
            Counted::No => self.refused = self.refused.saturating_sub(1),
        }
        Some(c)
    }

    /// A counted connection from `peer` (canonical) is gone: free its
    /// global and per-IP slots.
    fn release_slots(&mut self, peer: IpAddr) {
        self.counted = self.counted.saturating_sub(1);
        if let Some(n) = self.counted_per_ip.get_mut(&peer) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.counted_per_ip.remove(&peer);
            }
        }
    }

    /// The disk answered an orphaned open: the slots it held are free.
    fn release_orphan(&mut self, file: FileId) {
        if let Owner::Orphan(slots) = self.files.owner(file) {
            self.files.release(file);
            match slots {
                OrphanSlots::Held { peer } => self.release_slots(peer),
                OrphanSlots::None => {}
            }
        }
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
            Owner::Orphan(_) => {
                self.release_orphan(file);
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
            slash,
            step,
            body,
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
        let outcome = match (meta.kind, step, slash) {
            (FileKind::Regular, OpenStep::Requested, TrailingSlash::Absent)
            | (
                FileKind::Regular,
                OpenStep::Index,
                TrailingSlash::Present | TrailingSlash::Absent,
            ) => Opened::Serve,
            (FileKind::Directory, OpenStep::Requested, TrailingSlash::Absent) => {
                Opened::NotServed(NotServed::Redirect)
            }
            (FileKind::Directory, OpenStep::Requested, TrailingSlash::Present) => Opened::OpenIndex,
            (FileKind::Regular, OpenStep::Requested, TrailingSlash::Present)
            | (
                FileKind::Directory,
                OpenStep::Index,
                TrailingSlash::Present | TrailingSlash::Absent,
            )
            | (
                FileKind::Other,
                OpenStep::Requested | OpenStep::Index,
                TrailingSlash::Present | TrailingSlash::Absent,
            ) => Opened::NotServed(NotServed::NotFound),
        };
        match outcome {
            Opened::Serve => {
                let size = meta.stamp.size;
                let request = c.static_request.take();
                let on_change = request
                    .as_ref()
                    .map_or(OnFileChange::Abort, |r| r.on_change);
                let name = request
                    .as_ref()
                    .and_then(|r| r.path.last())
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let content_type = content_type(name);
                actions.push(Action::Log(Log::Response {
                    conn,
                    peer: c.peer,
                    status: Status::Ok,
                    request: request.map(|r| r.summary),
                }));
                render_file_head(content_type, size, persistence, &mut c.send);
                match body {
                    BodyMode::Full => {
                        let chunk = if size == 0 {
                            Chunk::Done
                        } else {
                            let end = size.min(FILE_CHUNK);
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
                            opened: meta.stamp,
                            on_change,
                            offset: 0,
                            chunk,
                            persistence,
                            buffered,
                            deadline,
                        };
                    }
                    // HEAD: the head is all; nothing is read.
                    BodyMode::HeadOnly => {
                        self.files.release(file);
                        actions.push(Action::CloseFile { file });
                        c.state = ConnState::Responding {
                            persistence,
                            unread_body: UnreadBody::None,
                            buffered,
                            deadline,
                        };
                    }
                }
            }
            Opened::OpenIndex => {
                // One file per connection: the directory goes first.
                self.files.release(file);
                actions.push(Action::CloseFile { file });
                match c.static_request.as_mut() {
                    Some(request) => {
                        request.path.push(INDEX.to_vec());
                        let next = self.files.allocate(conn);
                        actions.push(Action::OpenFile {
                            file: next,
                            root: request.root,
                            path: request.path.clone(),
                        });
                        c.state = ConnState::OpeningFile {
                            file: next,
                            slash,
                            step: OpenStep::Index,
                            body,
                            persistence,
                            buffered,
                            deadline,
                        };
                    }
                    // Every open has its request; answer 404 should it not.
                    None => not_served(
                        c,
                        conn,
                        NotServed::NotFound,
                        body,
                        persistence,
                        buffered,
                        deadline,
                        actions,
                    ),
                }
            }
            Opened::NotServed(reason) => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
                not_served(
                    c,
                    conn,
                    reason,
                    body,
                    persistence,
                    buffered,
                    deadline,
                    actions,
                );
            }
        }
        self.sync_interest(conn, actions);
    }

    /// An open, readahead or sendfile of `file` failed. A path that cannot
    /// be opened is answered 404. After the head was queued, the transfer is
    /// aborted: the file is closed and the connection cut. An answer an open
    /// cannot give closes the connection.
    fn file_failed(
        &mut self,
        now: Instant,
        file: FileId,
        error: FileError,
        actions: &mut Vec<Action>,
    ) {
        let conn = match self.files.owner(file) {
            Owner::Conn(conn) => conn,
            Owner::Orphan(_) => {
                self.release_orphan(file);
                return;
            }
            Owner::Unknown => return,
        };
        let deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        if c.state.file() != Some(file) {
            return;
        }
        if let ConnState::OpeningFile {
            body,
            persistence,
            buffered,
            ..
        } = c.state
        {
            match error {
                // Missing, symlinked, denied or otherwise unopenable: all
                // look the same to the client.
                FileError::NotFound
                | FileError::Symlink
                | FileError::PermissionDenied
                | FileError::Other => {
                    self.files.release(file);
                    not_served(
                        c,
                        conn,
                        NotServed::NotFound,
                        body,
                        persistence,
                        buffered,
                        deadline,
                        actions,
                    );
                    self.sync_interest(conn, actions);
                    return;
                }
                // Not an answer an open can give: treat as a broken driver.
                FileError::Truncated | FileError::Changed => {}
            }
        }
        match c.state {
            ConnState::OpeningFile { .. } => {
                if self.forget(conn, actions).is_some() {
                    // A failed open leaves nothing open to close, and its
                    // answer is in: the slot `forget` kept for it is free.
                    self.release_orphan(file);
                    actions.push(Action::Close { conn });
                }
            }
            ConnState::SendingFile { .. } => self.abort_transfer(conn, error, actions),
            // `c.state.file()` is `Some(file)`: unreachable.
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::Lingering { .. } => {}
        }
    }

    /// A file response failed after its head was queued: close the file,
    /// log why, and cut the connection at once, so the client sees an
    /// incomplete body rather than a short one that looks complete.
    fn abort_transfer(&mut self, conn: ConnId, error: FileError, actions: &mut Vec<Action>) {
        if let Some(c) = self.forget(conn, actions) {
            actions.push(Action::Log(Log::FileAborted {
                conn,
                peer: c.peer,
                error,
            }));
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
            opened,
            on_change,
            chunk: Chunk::Reading { end },
            ..
        } = c.state
        else {
            return;
        };
        if sending != file {
            return;
        }
        if let Err(error) = check_stamp(on_change, opened, stamp) {
            self.abort_transfer(conn, error, actions);
            return;
        }
        if let ConnState::SendingFile {
            chunk,
            deadline: state_deadline,
            ..
        } = &mut c.state
        {
            *chunk = Chunk::Ready { end };
            *state_deadline = deadline;
        }
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
            chunk: Chunk::Ready { end },
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
        let next_chunk = if offset < end {
            Chunk::Ready { end }
        } else if end < size {
            let next = size.min(end.saturating_add(FILE_CHUNK));
            actions.push(Action::Readahead {
                file,
                offset: end,
                len: next.saturating_sub(end),
            });
            Chunk::Reading { end: next }
        } else {
            Chunk::Done
        };
        match next_chunk {
            Chunk::Done => {
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
            Chunk::Ready { .. } | Chunk::Reading { .. } => {
                if let ConnState::SendingFile {
                    offset: state_offset,
                    chunk,
                    deadline: state_deadline,
                    ..
                } = &mut c.state
                {
                    *state_offset = offset;
                    *chunk = next_chunk;
                    *state_deadline = deadline;
                }
            }
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
                self.forget(conn, actions);
                actions.push(Action::Close { conn });
            }
            // The client stopped reading, or the disk did not answer in
            // time: close without further writes. An open in flight becomes
            // an orphan; an open file is closed.
            ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. } => {
                let kind = match c.state {
                    ConnState::OpeningFile { .. }
                    | ConnState::SendingFile {
                        chunk: Chunk::Reading { .. },
                        ..
                    } => TimeoutKind::Disk,
                    ConnState::SendingFile {
                        chunk: Chunk::Ready { .. } | Chunk::Done,
                        ..
                    }
                    | ConnState::Responding { .. }
                    | ConnState::ReadingHead { .. }
                    | ConnState::Idle { .. }
                    | ConnState::Lingering { .. } => TimeoutKind::Send,
                };
                actions.push(Action::Log(Log::Rejected {
                    conn,
                    peer: c.peer,
                    reason: RejectReason::Timeout(kind),
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
                // Once the request line is valid the method is known: a HEAD
                // request's error is head-only too. An error in the request
                // line itself keeps its body.
                let body = match parser.method() {
                    KnownMethod::Head => BodyMode::HeadOnly,
                    KnownMethod::Unknown | KnownMethod::Get | KnownMethod::Other => BodyMode::Full,
                };
                render_fixed(error.status(), Persistence::Close, body, &mut c.send);
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
        // Every render into the send buffer is bounded (fixed responses and
        // file heads by construction, see `every_rendered_head_fits_the_send_buffer`;
        // redirects by an explicit check), and this runs after every one.
        debug_assert!(
            c.send.len() <= SEND_CAPACITY,
            "send buffer holds {} bytes, beyond its capacity",
            c.send.len()
        );
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

/// The name served for a directory requested with a trailing `/`.
const INDEX: &[u8] = b"index.html";

/// What became of an opened static path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opened {
    /// A regular file to send (or, for HEAD, describe).
    Serve,
    /// A directory requested with `/`: open its `index.html` next.
    OpenIndex,
    NotServed(NotServed),
}

/// Why a static request is answered without a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotServed {
    /// A directory requested without `/`: 301 to the path plus `/`.
    Redirect,
    NotFound,
}

/// Close out `c`'s static request without a file: a 301 to the slash form
/// of its path (query kept), or a 404. Logged once, with the raw path.
#[allow(
    clippy::too_many_arguments,
    reason = "the state fields of the connection being answered"
)]
fn not_served(
    c: &mut Conn,
    conn: ConnId,
    reason: NotServed,
    body: BodyMode,
    persistence: Persistence,
    buffered: usize,
    deadline: Instant,
    actions: &mut Vec<Action>,
) {
    let request = c.static_request.take();
    let status = match (reason, &request) {
        (NotServed::Redirect, Some(request)) => {
            let mut location = format!("{}/", request.summary.path);
            if let Some(query) = &request.query {
                location.push('?');
                location.push_str(query);
            }
            // The send buffer never grows beyond its capacity: a redirect
            // too long for it (path and query are bounded only by
            // `max_request_line`) is answered 414 instead.
            let len = redirect_len(location.len(), persistence, body);
            if c.send.len().saturating_add(len) <= SEND_CAPACITY {
                render_redirect(&location, persistence, body, &mut c.send);
                Status::MovedPermanently
            } else {
                render_fixed(Status::UriTooLong, persistence, body, &mut c.send);
                Status::UriTooLong
            }
        }
        (NotServed::Redirect, None) | (NotServed::NotFound, Some(_) | None) => {
            render_fixed(Status::NotFound, persistence, body, &mut c.send);
            Status::NotFound
        }
    };
    actions.push(Action::Log(Log::Response {
        conn,
        peer: c.peer,
        status,
        request: request.map(|r| r.summary),
    }));
    c.state = ConnState::Responding {
        persistence,
        unread_body: UnreadBody::None,
        buffered,
        deadline,
    };
}

/// How a request on a matched route is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Plan {
    /// Open `path` (relative segments) beneath static root `root`; the
    /// route's change policy applies while it is sent.
    Open {
        root: RootId,
        path: Vec<Vec<u8>>,
        on_change: OnFileChange,
    },
    /// Answer `status` right away.
    Respond(Status, Persistence),
}

/// The static route rules that need no file: method, body, path.
fn plan(route: &Route, head: &RequestHead, static_roots: &[PathBuf]) -> Plan {
    let (root, on_change) = match &route.action {
        RouteAction::Static {
            root,
            on_file_change,
        } => (root, *on_file_change),
        // Not implemented yet in this stage: answered 501 and closed.
        RouteAction::Proxy(_) => return Plan::Respond(Status::NotImplemented, Persistence::Close),
    };
    match &head.method {
        Method::Get | Method::Head => {}
        Method::Other(_) => return Plan::Respond(Status::MethodNotAllowed, head.persistence),
    }
    match head.body {
        BodyFraming::None => {}
        BodyFraming::Length(_) | BodyFraming::Chunked => {
            return Plan::Respond(Status::ContentTooLarge, Persistence::Close);
        }
    }
    let Some(root) = static_roots
        .iter()
        .position(|r| r == root)
        .and_then(|index| u32::try_from(index).ok())
        .map(RootId)
    else {
        // Every configured root is listed; unreachable in practice.
        return Plan::Respond(Status::NotFound, head.persistence);
    };
    // Dotfiles (and paths no file can have) look like missing files.
    match relative_segments(&head.target.path, route.segments.len()) {
        Ok(path) => Plan::Open {
            root,
            path,
            on_change,
        },
        Err(_) => Plan::Respond(Status::NotFound, head.persistence),
    }
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
    // Every response to a HEAD request is head only.
    let body = match head.method {
        Method::Head => BodyMode::HeadOnly,
        Method::Get | Method::Other(_) => BodyMode::Full,
    };
    let (status, persistence) = match route {
        Err(_) => (Status::NotFound, head.persistence),
        Ok(route) => match plan(route, &head, static_roots) {
            Plan::Open {
                root,
                path,
                on_change,
            } => {
                let slash = trailing_slash(&head.target.path);
                let file = files.allocate(conn);
                actions.push(Action::OpenFile {
                    file,
                    root,
                    path: path.clone(),
                });
                c.state = ConnState::OpeningFile {
                    file,
                    slash,
                    step: OpenStep::Requested,
                    body,
                    persistence: head.persistence,
                    buffered: leftover,
                    deadline: send_deadline,
                };
                c.static_request = Some(StaticRequest {
                    summary: RequestSummary {
                        method: head.method,
                        host: head.host,
                        path: head.target.path,
                    },
                    query: head.target.query,
                    root,
                    on_change,
                    path,
                });
                return;
            }
            Plan::Respond(status, persistence) => (status, persistence),
        },
    };
    // An unread body leaves the stream unframed: it cannot be reused.
    let persistence = match unread_body {
        UnreadBody::None => persistence,
        UnreadBody::Pending => Persistence::Close,
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

    /// The send buffer bound: every fixed response and every file head,
    /// in every persistence and body mode, at its largest (longest content
    /// type, largest length), fits `SEND_CAPACITY` on its own. (Redirects
    /// are checked before they are rendered.)
    #[test]
    fn every_rendered_head_fits_the_send_buffer() {
        use crate::http::status::Status as S;
        let statuses = [
            S::Ok,
            S::MovedPermanently,
            S::BadRequest,
            S::NotFound,
            S::MethodNotAllowed,
            S::RequestTimeout,
            S::ContentTooLarge,
            S::UriTooLong,
            S::RequestHeaderFieldsTooLarge,
            S::NotImplemented,
            S::ServiceUnavailable,
            S::HttpVersionNotSupported,
        ];
        for persistence in [Persistence::KeepAlive, Persistence::Close] {
            for body in [BodyMode::Full, BodyMode::HeadOnly] {
                for status in statuses {
                    let mut out = Vec::new();
                    render_fixed(status, persistence, body, &mut out);
                    assert!(out.len() <= SEND_CAPACITY, "{status:?}");
                }
            }
            let mut out = Vec::new();
            // Far longer than any content type in the table.
            let content_type = "x".repeat(200);
            render_file_head(&content_type, u64::MAX, persistence, &mut out);
            assert!(out.len() <= SEND_CAPACITY);
        }
    }

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
    fn directory_without_slash_is_closed_and_redirected() {
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
        assert_eq!(
            actions,
            [
                Action::CloseFile { file },
                Action::Log(Log::Response {
                    conn,
                    peer: PEER,
                    status: Status::MovedPermanently,
                    request: Some(RequestSummary {
                        method: Method::Get,
                        host: Some("a.test".to_owned()),
                        path: "/x/f.txt".to_owned(),
                    }),
                }),
                Action::SetInterest {
                    conn,
                    interest: Interest::Write
                },
            ]
        );
        let mut expected = Vec::new();
        crate::http::response::render_redirect(
            "/x/f.txt/",
            Persistence::KeepAlive,
            BodyMode::Full,
            &mut expected,
        );
        assert_eq!(core.send_buffer(conn), expected.as_slice());
        assert_eq!(core.files.len(), 0);
    }

    #[test]
    fn directory_with_slash_opens_its_index_after_closing_it() {
        let mut core = files_core();
        let now = secs(0);
        let conn = ConnId(1);
        request(
            &mut core,
            now,
            1,
            b"GET /x/d/ HTTP/1.1\r\nHost: a.test\r\n\r\n",
        );
        let mut actions = Vec::new();
        let directory = FileMeta {
            kind: FileKind::Directory,
            stamp: stamp(0),
        };
        let file = FileId(1);
        let meta = directory;
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::CloseFile { file },
                Action::OpenFile {
                    file: FileId(2),
                    root: RootId(1),
                    path: vec![b"d".to_vec(), b"index.html".to_vec()],
                },
            ]
        );
        assert!(core.send_buffer(conn).is_empty());
        // The index is a directory too: 404.
        actions.clear();
        let file = FileId(2);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        assert_eq!(actions.first(), Some(&Action::CloseFile { file }));
        assert!(
            actions.iter().any(|a| matches!(
                a,
                Action::Log(Log::Response {
                    status: Status::NotFound,
                    ..
                })
            )),
            "{actions:?}"
        );
        let mut expected = Vec::new();
        render_fixed(
            Status::NotFound,
            Persistence::KeepAlive,
            BodyMode::Full,
            &mut expected,
        );
        assert_eq!(core.send_buffer(conn), expected.as_slice());
        assert_eq!(core.files.len(), 0);
    }

    #[test]
    fn head_of_a_file_sends_the_head_and_closes_it() {
        let mut core = files_core();
        let now = secs(0);
        let conn = ConnId(1);
        let file = FileId(1);
        request(
            &mut core,
            now,
            1,
            b"HEAD /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n",
        );
        let mut actions = Vec::new();
        let meta = regular(1 << 40);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::Log(Log::Response {
                    conn,
                    peer: PEER,
                    status: Status::Ok,
                    request: Some(RequestSummary {
                        method: Method::Head,
                        host: Some("a.test".to_owned()),
                        path: "/x/f.txt".to_owned(),
                    }),
                }),
                Action::CloseFile { file },
                Action::SetInterest {
                    conn,
                    interest: Interest::Write
                },
            ]
        );
        let mut head = Vec::new();
        crate::http::response::render_file_head(
            "text/plain; charset=utf-8",
            1 << 40,
            Persistence::KeepAlive,
            &mut head,
        );
        assert_eq!(core.send_buffer(conn), head.as_slice());
        assert_eq!(core.send_file(conn), None);
        actions.clear();
        let len = head.len();
        core.handle(now, Event::Sent { conn, len }, &mut actions);
        assert_eq!(
            actions,
            [Action::SetInterest {
                conn,
                interest: Interest::Read
            }]
        );
        assert_eq!(core.files.len(), 0);
    }

    #[test]
    fn file_failure_closes_the_connection_and_its_file() {
        let mut core = files_core();
        let now = secs(0);
        // Failed open: 404, nothing to close, the connection stays.
        let file = FileId(1);
        let conn = ConnId(1);
        let mut actions = Vec::new();
        for error in [
            FileError::NotFound,
            FileError::Symlink,
            FileError::PermissionDenied,
            FileError::Other,
        ] {
            let mut core = files_core();
            request(&mut core, now, 1, GET_F);
            actions.clear();
            core.handle(now, Event::FileFailed { file, error }, &mut actions);
            assert_eq!(
                actions,
                [
                    Action::Log(Log::Response {
                        conn,
                        peer: PEER,
                        status: Status::NotFound,
                        request: Some(RequestSummary {
                            method: Method::Get,
                            host: Some("a.test".to_owned()),
                            path: "/x/f.txt".to_owned(),
                        }),
                    }),
                    Action::SetInterest {
                        conn,
                        interest: Interest::Write
                    },
                ],
                "{error:?}"
            );
            assert_eq!(core.files.len(), 0);
            assert_eq!(core.counted, 1);
        }
        // Failed readahead: the open file is closed too.
        let conn = ConnId(2);
        request(&mut core, now, 2, GET_F);
        let meta = regular(3);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        actions.clear();
        let error = FileError::Other;
        core.handle(now, Event::FileFailed { file, error }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::CloseFile { file },
                Action::Log(Log::FileAborted {
                    conn,
                    peer: PEER,
                    error
                }),
                Action::Close { conn }
            ]
        );
        assert!(core.conns.is_empty());
        assert_eq!(core.counted, 0);
        assert!(core.counted_per_ip.is_empty());
        assert_eq!(core.files.len(), 0);
    }

    /// `Truncated` or `Changed` cannot answer an `OpenFile`: the driver is
    /// broken, so the connection is closed (nothing is open to close).
    #[test]
    fn impossible_open_errors_close_the_connection() {
        let now = secs(0);
        let file = FileId(1);
        let conn = ConnId(1);
        for error in [FileError::Truncated, FileError::Changed] {
            let mut core = files_core();
            request(&mut core, now, 1, GET_F);
            let mut actions = Vec::new();
            core.handle(now, Event::FileFailed { file, error }, &mut actions);
            assert_eq!(actions, [Action::Close { conn }], "{error:?}");
            assert!(core.conns.is_empty());
            assert_eq!(core.counted, 0);
            assert!(core.counted_per_ip.is_empty());
            assert_eq!(core.files.len(), 0);
        }
    }

    /// `send_idle_timeout` passing while the open or a readahead is in
    /// flight is a disk timeout; while a ready chunk waits for the client,
    /// a send timeout.
    #[test]
    fn disk_and_send_timeouts_while_sending_a_file() {
        let now = secs(0);
        let conn = ConnId(1);
        let file = FileId(1);
        let rejected = |kind| {
            Action::Log(Log::Rejected {
                conn,
                peer: PEER,
                reason: RejectReason::Timeout(kind),
            })
        };
        // Waiting on the open: the open becomes an orphan.
        let mut core = files_core();
        request(&mut core, now, 1, GET_F);
        let mut actions = Vec::new();
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert_eq!(
            actions,
            [rejected(TimeoutKind::Disk), Action::Close { conn }]
        );
        assert_eq!(core.counted, 1);
        // Waiting on a readahead: the file is closed.
        let mut core = files_core();
        request(&mut core, now, 1, GET_F);
        let meta = regular(3);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        let len = core.send_buffer(conn).len();
        core.handle(now, Event::Sent { conn, len }, &mut actions);
        actions.clear();
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert_eq!(
            actions,
            [
                rejected(TimeoutKind::Disk),
                Action::CloseFile { file },
                Action::Close { conn }
            ]
        );
        assert_eq!(core.counted, 0);
        assert_eq!(core.files.len(), 0);
        // A ready chunk the client does not take: a send timeout.
        let mut core = files_core();
        request(&mut core, now, 1, GET_F);
        core.handle(now, Event::FileOpened { file, meta }, &mut actions);
        let stamp = stamp(3);
        core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);
        actions.clear();
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert_eq!(
            actions,
            [
                rejected(TimeoutKind::Send),
                Action::CloseFile { file },
                Action::Close { conn }
            ]
        );
        assert_eq!(core.files.len(), 0);
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
    fn static_rules_decided_before_opening() {
        let mut core = files_core();
        let now = secs(0);
        for (id, req, status) in [
            (
                1,
                b"HEAD /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n".as_slice(),
                None,
            ),
            (
                2,
                b"POST /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n",
                Some(Status::MethodNotAllowed),
            ),
            (
                3,
                b"GET /x/.f HTTP/1.1\r\nHost: a.test\r\n\r\n",
                Some(Status::NotFound),
            ),
            (
                4,
                b"GET /x/f HTTP/1.1\r\nHost: a.test\r\nContent-Length: 1\r\n\r\nx",
                Some(Status::ContentTooLarge),
            ),
            (
                5,
                b"GET /p/f HTTP/1.1\r\nHost: b.test\r\n\r\n",
                Some(Status::NotImplemented),
            ),
        ] {
            let actions = request(&mut core, now, id, req);
            let opened = actions.iter().any(|a| matches!(a, Action::OpenFile { .. }));
            let logged = actions.iter().find_map(|a| {
                if let Action::Log(Log::Response { status, .. }) = a {
                    Some(*status)
                } else {
                    None
                }
            });
            assert_eq!(opened, status.is_none(), "{id}: {actions:?}");
            assert_eq!(logged, status, "{id}: {actions:?}");
        }
    }

    /// An orphaned open keeps its connection slot until the disk answers;
    /// the per-IP slot is released at close.
    #[test]
    fn orphaned_opens_count_until_the_disk_answers() {
        let mut core = files_core();
        let now = secs(0);
        let mut actions = Vec::new();
        // conn 1: closed by the peer, then the open succeeds.
        request(&mut core, now, 1, GET_F);
        core.handle(now, Event::ReadClosed { conn: ConnId(1) }, &mut actions);
        // conn 2: reset, then the open fails.
        request(&mut core, now, 2, GET_F);
        let error = ConnError::Reset;
        core.handle(
            now,
            Event::Failed {
                conn: ConnId(2),
                error,
            },
            &mut actions,
        );
        // conn 3: times out waiting on the disk, then the open succeeds.
        request(&mut core, now, 3, GET_F);
        core.handle(secs(30), Event::TimerFired, &mut actions);
        assert!(core.conns.is_empty());
        assert_eq!(core.counted, 3);
        // The per-IP slots are held too, so one peer cannot take every
        // global slot with orphans.
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&3));
        assert_eq!(core.next_deadline(), None);
        let meta = regular(3);
        actions.clear();
        core.handle(
            now,
            Event::FileOpened {
                file: FileId(1),
                meta,
            },
            &mut actions,
        );
        assert_eq!(actions, [Action::CloseFile { file: FileId(1) }]);
        assert_eq!(core.counted, 2);
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&2));
        let error = FileError::NotFound;
        core.handle(
            now,
            Event::FileFailed {
                file: FileId(2),
                error,
            },
            &mut actions,
        );
        assert_eq!(core.counted, 1);
        assert_eq!(core.counted_per_ip.get(&PEER), Some(&1));
        core.handle(
            now,
            Event::FileOpened {
                file: FileId(3),
                meta,
            },
            &mut actions,
        );
        assert_eq!(core.counted, 0);
        assert!(core.counted_per_ip.is_empty());
        assert_eq!(core.files.len(), 0);
        // Repeated answers change nothing.
        core.handle(
            now,
            Event::FileOpened {
                file: FileId(3),
                meta,
            },
            &mut actions,
        );
        assert_eq!(core.counted, 0);
    }

    /// A disk timeout orphans the open; its connection slot comes back
    /// with the late answer, a `FileOpened` (closed at once) or a
    /// `FileFailed` (nothing to close) alike.
    #[test]
    fn disk_timeout_orphan_slot_is_released_by_any_late_answer() {
        let file = FileId(1);
        let conn = ConnId(1);
        let late_answers = [
            Event::FileOpened {
                file,
                meta: regular(3),
            },
            Event::FileFailed {
                file,
                error: FileError::NotFound,
            },
        ];
        for late in late_answers {
            let mut core = files_core();
            request(&mut core, secs(0), 1, GET_F);
            let mut actions = Vec::new();
            core.handle(secs(30), Event::TimerFired, &mut actions);
            assert_eq!(
                actions,
                [
                    Action::Log(Log::Rejected {
                        conn,
                        peer: PEER,
                        reason: RejectReason::Timeout(TimeoutKind::Disk),
                    }),
                    Action::Close { conn }
                ]
            );
            assert!(core.conns.is_empty());
            assert_eq!(core.counted, 1, "{late:?}");
            assert_eq!(core.counted_per_ip.get(&PEER), Some(&1), "{late:?}");
            assert_eq!(core.files.len(), 1);
            actions.clear();
            let opened = matches!(late, Event::FileOpened { .. });
            core.handle(secs(31), late, &mut actions);
            if opened {
                assert_eq!(actions, [Action::CloseFile { file }]);
            } else {
                assert!(actions.is_empty(), "{actions:?}");
            }
            assert_eq!(core.counted, 0);
            assert!(core.counted_per_ip.is_empty(), "opened: {opened}");
            assert_eq!(core.files.len(), 0);
            assert_eq!(core.next_deadline(), None);
        }
    }
}
