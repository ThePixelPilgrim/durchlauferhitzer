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
//! Rules for files:
//!
//! - The core allocates [`FileId`]s, sequentially from 1, and never reuses
//!   one. A [`RootId`] indexes [`Core::static_roots`]: the driver opens each
//!   root once at startup.
//! - `OpenFile { root, path }` names a path as relative segments (decoded
//!   bytes, never `.`, `..`, empty, or containing `/` or NUL). The driver
//!   resolves it beneath the root, without following symlinks anywhere in
//!   the path and without ever leaving the root (`openat2` with
//!   `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`), and reports the metadata
//!   (`fstat`) of the open descriptor. An empty segment list means the root
//!   itself: the driver opens `"."` beneath the root descriptor
//!   (`openat2(root, "")` fails with `ENOENT`, which would make `GET /` a
//!   404).
//! - Open flags: `O_RDONLY | O_NONBLOCK | O_NOCTTY | O_CLOEXEC`, never
//!   `O_CREAT`, `O_TRUNC` or a write mode. `O_NONBLOCK` is required: a FIFO
//!   in a root opened without it blocks the opening worker until a writer
//!   appears, before any `fstat` could tell it is not a regular file. The
//!   `RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH` resolve flags take the place of
//!   `O_NOFOLLOW` for every component. `RESOLVE_NO_XDEV` is not set: mount
//!   points beneath a root are the operator's choice. Hard links inside a
//!   root cannot be detected and are the operator's responsibility.
//! - Opens may block (a cold dentry or inode cache), like readahead, so
//!   the driver runs `openat2` and `fstat` on its worker pool too, never on
//!   the event loop. Answers for different files may arrive in any order.
//! - Errors map to [`FileError`] as follows (all of them become a 404, so
//!   the mapping only matters for logs):
//!
//!   | errno                                      | `FileError`        |
//!   |--------------------------------------------|--------------------|
//!   | `ENOENT`, `ENOTDIR`                        | `NotFound`         |
//!   | `ELOOP`, `EXDEV` (refused by `RESOLVE_*`)  | `Symlink`          |
//!   | `EACCES`, `EPERM`                          | `PermissionDenied` |
//!   | anything else (`EIO`, `ENAMETOOLONG`, ...) | `Other`            |
//!
//!   `EMFILE`/`ENFILE` also map to `Other`; the driver logs them, since
//!   they mean the descriptor budget below is wrong, not a missing file.
//! - The stamp is `size = st_size` and `modified_ns = st_mtime` in
//!   nanoseconds (`tv_sec * 10^9 + tv_nsec`); an mtime before 1970 clamps to
//!   0, and an overflowing one saturates. The mtime granularity (often one
//!   kernel tick) means a rewrite of the same size within one tick goes
//!   unnoticed, and stamps are only compared at chunk boundaries, so a
//!   change racing the last chunk's `sendfile` is never detected.
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
//!   did send. Other `sendfile` errors: `EAGAIN` means wait for writability
//!   (not reported); `EPIPE`/`ECONNRESET` are the socket's, reported as
//!   `Failed { conn, error: Reset }`; `EIO` and other file-side errors are
//!   reported as `FileFailed { error: Other }`.
//! - Known limit: `sendfile` itself may block on the disk. Readahead is
//!   advisory, and pages can be evicted between `ChunkReady` and the
//!   `sendfile`, which then reads them on the event loop. This is accepted
//!   for now; mitigations (an `RWF_NOWAIT` probe, `mincore`) are stage-7
//!   work.
//! - Descriptor budget: open files are bounded by connections, since each
//!   holds at most one file id and an orphaned open keeps its connection's
//!   slot, so at most `max_connections` file descriptors are open or being
//!   opened at once, orphans included. The driver checks at startup that
//!   `RLIMIT_NOFILE` covers `2 × max_connections` client sockets (refused
//!   connections, at most `max_connections` of them, hold sockets too),
//!   `max_connections` files (orphaned opens included, since they keep
//!   their connection's slot), the static roots, its listeners and its
//!   eventfd (and, from stage 3, the backend sockets).
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

mod contract;
mod limits;
mod request;
mod static_files;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod timers;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use crate::config::{Config, RouteAction};
#[cfg(doc)]
use crate::conn::LINGER_TIMEOUT;
use crate::conn::{Conn, ConnState, LINGER_SCRATCH, SEND_CAPACITY};
use crate::file::{Chunk, FileSegment, FileTable};
#[cfg(doc)]
use crate::file::{FileError, FileId, RootId};
use crate::http::request::HeadLimits;
use crate::log::Log;
use crate::routing::Router;
use crate::time::Instant;

pub use contract::{Action, ConnError, ConnId, Event, Interest};

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
