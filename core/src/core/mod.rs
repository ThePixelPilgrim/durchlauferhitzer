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
//!   Likewise a write from `send_buffer(c)` is reported by its `Sent`, and a
//!   `sendfile` of the `send_file(c)` segment by its `FileSent` (or
//!   `FileFailed`), as the very next `handle` call after that write: the
//!   core may change the buffer or the segment in any call, so a count
//!   reported later could describe bytes that are no longer there.
//! - An **empty** `recv_buffer` means "do not read now" (unknown connection,
//!   not in a reading state, or buffer full). An empty `send_buffer` means
//!   "nothing to write". Neither is an error.
//! - The driver arms timers from [`Core::next_deadline`] after every `handle`
//!   call and delivers [`Event::TimerFired`] once that instant has passed.
//!   After `TimerFired`, every deadline at or before `now` has been handled:
//!   `next_deadline` is `None` or later than `now`.
//!
//! Rules for files are in [`static_files`], next to the code that
//! follows them.
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

mod static_files;

use static_files::{Plan, plan};
#[cfg(test)]
mod tests;

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
