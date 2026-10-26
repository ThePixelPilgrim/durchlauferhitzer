//! Answering requests from static roots: the static route rules, the
//! opens, and the file transfer.
//!
//! # Rules for files
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
//!   logs [`Log::FileAborted`], emits `CloseFile` and closes the connection
//!   at once, with no lingering and no further writes, so the client sees a
//!   cut body, never a complete-looking short one. A failed open is
//!   answered 404; `Truncated` or `Changed` in answer to `OpenFile` is a
//!   driver bug and closes the connection.
//! - A directory requested without its trailing `/` is answered 301 to the
//!   path plus `/` (query kept), unless that response would not fit the
//!   send buffer: then 414. The send buffer never holds more than its
//!   capacity.
//!
//! [`Interest::Write`]: super::Interest::Write
//! [`Interest::None`]: super::Interest::None

use std::path::PathBuf;

use super::{Action, ConnId, Core, finish};
use crate::config::{OnFileChange, Route, RouteAction};
use crate::conn::{Conn, ConnState, Exchange, OpenStep, SEND_CAPACITY, StaticRequest, UnreadBody};
use crate::file::{
    Chunk, FILE_CHUNK, FileError, FileId, FileKind, FileMeta, FileStamp, Owner, RootId, check_stamp,
};
use crate::http::mime::content_type;
use crate::http::request::{BodyFraming, Method, Persistence, RequestHead};
use crate::http::response::{
    BodyMode, redirect_len, render_file_head, render_fixed, render_redirect,
};
use crate::http::static_path::{TrailingSlash, relative_segments, trailing_slash};
use crate::http::status::Status;
use crate::log::Log;
use crate::time::Instant;

impl Core {
    /// Open the file `request` names on `conn`, which waits for it until
    /// `deadline`; `body` and `exchange` describe the response to come.
    pub(super) fn open_static(
        &mut self,
        conn: ConnId,
        request: StaticRequest,
        body: BodyMode,
        exchange: Exchange,
        deadline: Instant,
        actions: &mut Vec<Action>,
    ) {
        let Some(c) = self.conns.get_mut(&conn) else {
            // `try_parse` just found this connection: a miss is a core bug,
            // and with no connection there is nothing left to answer.
            debug_assert!(false, "open_static for unknown {conn:?}");
            return;
        };
        let slash = trailing_slash(&request.summary.path);
        let file = self.files.allocate(conn);
        actions.push(Action::OpenFile {
            file,
            root: request.root,
            path: request.path.clone(),
        });
        c.state = ConnState::OpeningFile {
            file,
            slash,
            step: OpenStep::Requested,
            body,
            exchange,
            request,
            deadline,
        };
    }

    /// The driver opened `file`: answer the request it belongs to.
    pub(super) fn file_opened(
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
        c.state = match c.take_state() {
            ConnState::OpeningFile {
                body,
                exchange,
                mut request,
                ..
            } => match outcome {
                Opened::Serve => {
                    let size = meta.stamp.size;
                    let name = request.path.last().map(Vec::as_slice).unwrap_or_default();
                    let content_type = content_type(name);
                    actions.push(Action::Log(Log::Response {
                        conn,
                        peer: c.peer,
                        status: Status::Ok,
                        request: Some(request.summary),
                    }));
                    render_file_head(content_type, size, exchange.persistence, &mut c.send);
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
                            ConnState::SendingFile {
                                file,
                                size,
                                opened: meta.stamp,
                                on_change: request.on_change,
                                offset: 0,
                                chunk,
                                exchange,
                                deadline,
                            }
                        }
                        // HEAD: the head is all; nothing is read.
                        BodyMode::HeadOnly => {
                            self.files.release(file);
                            actions.push(Action::CloseFile { file });
                            ConnState::responding(exchange, deadline)
                        }
                    }
                }
                Opened::OpenIndex => {
                    // One file per connection: the directory goes first.
                    self.files.release(file);
                    actions.push(Action::CloseFile { file });
                    request.path.push(INDEX.to_vec());
                    let next = self.files.allocate(conn);
                    actions.push(Action::OpenFile {
                        file: next,
                        root: request.root,
                        path: request.path.clone(),
                    });
                    ConnState::OpeningFile {
                        file: next,
                        slash,
                        step: OpenStep::Index,
                        body,
                        exchange,
                        request,
                        deadline,
                    }
                }
                Opened::NotServed(reason) => {
                    self.files.release(file);
                    actions.push(Action::CloseFile { file });
                    not_served(
                        c,
                        conn,
                        reason,
                        request,
                        body,
                        exchange.persistence,
                        actions,
                    );
                    ConnState::responding(exchange, deadline)
                }
            },
            // Checked above: unreachable, and left as it was.
            state @ (ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Lingering { .. }) => state,
        };
        self.sync_interest(conn, actions);
    }

    /// An open, readahead or sendfile of `file` failed. A path that cannot
    /// be opened is answered 404. After the head was queued, the transfer is
    /// aborted: the connection is cut and the file closed. An answer an open
    /// cannot give closes the connection.
    pub(super) fn file_failed(
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
        match c.state {
            ConnState::OpeningFile { .. } => match error {
                // Missing, symlinked, denied or otherwise unopenable: all
                // look the same to the client.
                FileError::NotFound
                | FileError::Symlink
                | FileError::PermissionDenied
                | FileError::Other => {
                    self.files.release(file);
                    c.state = match c.take_state() {
                        ConnState::OpeningFile {
                            body,
                            exchange,
                            request,
                            ..
                        } => {
                            let reason = NotServed::NotFound;
                            not_served(
                                c,
                                conn,
                                reason,
                                request,
                                body,
                                exchange.persistence,
                                actions,
                            );
                            ConnState::responding(exchange, deadline)
                        }
                        // Matched above: unreachable, and left as it was.
                        state @ (ConnState::ReadingHead { .. }
                        | ConnState::Idle { .. }
                        | ConnState::Responding { .. }
                        | ConnState::SendingFile { .. }
                        | ConnState::Lingering { .. }) => state,
                    };
                    self.sync_interest(conn, actions);
                }
                // Not an answer an open can give: treat as a broken driver.
                FileError::Truncated | FileError::Changed => {
                    if self.forget(conn, actions).is_some() {
                        // A failed open leaves nothing open to close, and its
                        // answer is in: the slot `forget` kept for it is free.
                        self.release_orphan(file);
                        actions.push(Action::Close { conn });
                    }
                }
            },
            ConnState::SendingFile { .. } => self.abort_transfer(conn, error, actions),
            // `c.state.file()` is `Some(file)`: unreachable.
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::Lingering { .. } => {}
        }
    }

    /// A file response failed after its head was queued: log why, close
    /// the file, and cut the connection at once, so the client sees an
    /// incomplete body rather than a short one that looks complete.
    fn abort_transfer(&mut self, conn: ConnId, error: FileError, actions: &mut Vec<Action>) {
        let Some(c) = self.conns.get(&conn) else {
            return;
        };
        actions.push(Action::Log(Log::FileAborted {
            conn,
            peer: c.peer,
            error,
        }));
        self.forget(conn, actions);
        actions.push(Action::Close { conn });
    }

    /// The readahead in flight for `file` completed.
    pub(super) fn chunk_ready(
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
    pub(super) fn file_sent(
        &mut self,
        now: Instant,
        conn: ConnId,
        len: u64,
        actions: &mut Vec<Action>,
    ) {
        let limits = &self.config.limits;
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let ConnState::SendingFile {
            file,
            size,
            offset,
            chunk: Chunk::Ready { end },
            exchange,
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
                c.state = finish(conn, c, exchange, UnreadBody::None, now, limits, actions);
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

/// Answer `c`'s static `request` without a file: a 301 to the slash form
/// of its path (query kept), or a 404. Logged once, with the raw path. The
/// caller moves `c` on to responding.
fn not_served(
    c: &mut Conn,
    conn: ConnId,
    reason: NotServed,
    request: StaticRequest,
    body: BodyMode,
    persistence: Persistence,
    actions: &mut Vec<Action>,
) {
    let status = match reason {
        NotServed::Redirect => {
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
        NotServed::NotFound => {
            render_fixed(Status::NotFound, persistence, body, &mut c.send);
            Status::NotFound
        }
    };
    actions.push(Action::Log(Log::Response {
        conn,
        peer: c.peer,
        status,
        request: Some(request.summary),
    }));
}

/// How a request on a matched route is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
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
pub(super) fn plan(route: &Route, head: &RequestHead, static_roots: &[PathBuf]) -> Plan {
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
