//! Static routes: the rules decided before opening, opening the path (and
//! a directory's index), sending the file chunk by chunk, and aborting a
//! transfer that fails.

use std::path::{Path, PathBuf};

use super::request::{AfterResponse, finish};
use super::{Action, ConnId, Core};
use crate::config::{OnFileChange, Route};
use crate::conn::{Conn, ConnState, OpenStep, SEND_CAPACITY, StaticRequest, UnreadBody};
use crate::file::{
    Chunk, FILE_CHUNK, FileError, FileId, FileKind, FileMeta, FileStamp, Owner, RootId, check_stamp,
};
use crate::http::date::ImfFixdate;
use crate::http::mime::content_type;
use crate::http::request::{BodyFraming, Method, Persistence, RequestHead};
use crate::http::response::{
    BodyMode, redirect_len, render_file_head, render_fixed, render_redirect,
};
use crate::http::static_path::{TrailingSlash, relative_segments, trailing_slash};
use crate::http::status::Status;
use crate::log::{Log, RequestSummary};
use crate::time::Instant;

impl Core {
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
        let Some(Opening {
            slash,
            step,
            body,
            persistence,
            buffered,
            mut request,
        }) = take_opening(c, file)
        else {
            return;
        };
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
        c.state = match outcome {
            Opened::Serve => {
                let size = meta.stamp.size;
                let on_change = request.on_change;
                let name = request.path.last().map(Vec::as_slice).unwrap_or_default();
                let content_type = content_type(name);
                actions.push(Action::Log(Log::Response {
                    conn,
                    peer: c.peer,
                    status: Status::Ok,
                    request: Some(request.summary),
                }));
                render_file_head(
                    content_type,
                    size,
                    persistence,
                    self.date.as_ref(),
                    &mut c.send,
                );
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
                            on_change,
                            offset: 0,
                            chunk,
                            persistence,
                            buffered,
                            deadline,
                        }
                    }
                    // HEAD: the head is all; nothing is read.
                    BodyMode::HeadOnly => {
                        self.files.release(file);
                        actions.push(Action::CloseFile { file });
                        ConnState::Responding {
                            persistence,
                            unread_body: UnreadBody::None,
                            buffered,
                            deadline,
                        }
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
                    persistence,
                    buffered,
                    deadline,
                    request,
                }
            }
            Opened::NotServed(reason) => {
                self.files.release(file);
                actions.push(Action::CloseFile { file });
                not_served(
                    c,
                    conn,
                    reason,
                    Reply {
                        request,
                        body,
                        persistence,
                        buffered,
                        date: self.date,
                    },
                    deadline,
                    actions,
                )
            }
        };
        self.sync_interest(conn, actions);
    }

    /// An open, readahead or sendfile of `file` failed. A path that cannot
    /// be opened is answered 404. After the head was queued, the transfer is
    /// aborted: the file is closed and the connection cut. An answer an open
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
        match error {
            // Missing, symlinked, denied or otherwise unopenable: all look
            // the same to the client.
            FileError::NotFound
            | FileError::Symlink
            | FileError::PermissionDenied
            | FileError::Other => {
                if let Some(Opening {
                    body,
                    persistence,
                    buffered,
                    request,
                    ..
                }) = take_opening(c, file)
                {
                    self.files.release(file);
                    c.state = not_served(
                        c,
                        conn,
                        NotServed::NotFound,
                        Reply {
                            request,
                            body,
                            persistence,
                            buffered,
                            date: self.date,
                        },
                        deadline,
                        actions,
                    );
                    self.sync_interest(conn, actions);
                    return;
                }
            }
            // Not an answer an open can give: treat as a broken driver.
            FileError::Truncated | FileError::Changed => {}
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
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. }
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
                    AfterResponse {
                        persistence,
                        unread_body: UnreadBody::None,
                        buffered,
                    },
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

/// The fields of a `ConnState::OpeningFile` taken out of a connection to
/// answer its open (its deadline is replaced by the next state's).
struct Opening {
    slash: TrailingSlash,
    step: OpenStep,
    body: BodyMode,
    persistence: Persistence,
    buffered: usize,
    request: StaticRequest,
}

/// If `c` is opening `file`, take that state out so its request can move
/// on; the caller then sets `c`'s next state. Any other state stays.
#[must_use]
fn take_opening(c: &mut Conn, file: FileId) -> Option<Opening> {
    match c.take_state() {
        ConnState::OpeningFile {
            file: opening,
            slash,
            step,
            body,
            persistence,
            buffered,
            deadline: _,
            request,
        } if opening == file => Some(Opening {
            slash,
            step,
            body,
            persistence,
            buffered,
            request,
        }),
        other @ (ConnState::OpeningFile { .. }
        | ConnState::ReadingHead { .. }
        | ConnState::Idle { .. }
        | ConnState::Responding { .. }
        | ConnState::SendingFile { .. }
        | ConnState::Connecting { .. }
        | ConnState::AwaitingResponse { .. }
        | ConnState::Relaying { .. }
        | ConnState::Lingering { .. }) => {
            c.state = other;
            None
        }
    }
}

/// What answering a static request without a file needs: the request and
/// how its connection responds.
struct Reply {
    request: StaticRequest,
    body: BodyMode,
    persistence: Persistence,
    buffered: usize,
    /// The core's `Date` value, if the driver set one.
    date: Option<ImfFixdate>,
}

/// Close out the static request of `reply` on `c` without a file: a 301 to
/// the slash form of its path (query kept), or a 404. Logged once, with the
/// raw path. Returns the connection's next state.
fn not_served(
    c: &mut Conn,
    conn: ConnId,
    reason: NotServed,
    reply: Reply,
    deadline: Instant,
    actions: &mut Vec<Action>,
) -> ConnState {
    let Reply {
        request,
        body,
        persistence,
        buffered,
        date,
    } = reply;
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
            let len = redirect_len(location.len(), persistence, body, date.as_ref());
            if c.send.len().saturating_add(len) <= SEND_CAPACITY {
                render_redirect(&location, persistence, body, date.as_ref(), &mut c.send);
                Status::MovedPermanently
            } else {
                render_fixed(
                    Status::UriTooLong,
                    persistence,
                    body,
                    date.as_ref(),
                    &mut c.send,
                );
                Status::UriTooLong
            }
        }
        NotServed::NotFound => {
            render_fixed(
                Status::NotFound,
                persistence,
                body,
                date.as_ref(),
                &mut c.send,
            );
            Status::NotFound
        }
    };
    actions.push(Action::Log(Log::Response {
        conn,
        peer: c.peer,
        status,
        request: Some(request.summary),
    }));
    ConnState::Responding {
        persistence,
        unread_body: UnreadBody::None,
        buffered,
        deadline,
    }
}

/// How a request on a matched route is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Plan {
    /// Open a path beneath a static root.
    Open(StaticOpen),
    /// Answer `status` right away.
    Respond(Status, Persistence),
}

/// Open `path` (relative segments) beneath static root `root`; the route's
/// change policy applies while it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StaticOpen {
    root: RootId,
    path: Vec<Vec<u8>>,
    on_change: OnFileChange,
}

/// The rules of static `route` (serving `root` under `on_change`) that
/// need no file: method, body, path.
pub(super) fn plan(
    route: &Route,
    root: &Path,
    on_change: OnFileChange,
    head: &RequestHead,
    static_roots: &[PathBuf],
) -> Plan {
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
        Ok(path) => Plan::Open(StaticOpen {
            root,
            path,
            on_change,
        }),
        Err(_) => Plan::Respond(Status::NotFound, head.persistence),
    }
}

impl Core {
    /// Start answering `head` on `conn` from a static root: open the
    /// requested path. The connection then waits on the disk until the send
    /// idle deadline; `leftover` bytes at the front of its receive buffer
    /// wait for the next request.
    pub(super) fn open_static(
        &mut self,
        now: Instant,
        conn: ConnId,
        head: RequestHead,
        open: StaticOpen,
        leftover: usize,
        actions: &mut Vec<Action>,
    ) {
        let send_deadline = now.saturating_add(self.config.limits.send_idle_timeout);
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let body = BodyMode::of(&head.method);
        let StaticOpen {
            root,
            path,
            on_change,
        } = open;
        let slash = trailing_slash(&head.target.path);
        let file = self.files.allocate(conn);
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
            request: StaticRequest {
                summary: RequestSummary {
                    method: head.method,
                    host: head.host,
                    path: head.target.path,
                },
                query: head.target.query,
                root,
                on_change,
                path,
            },
        };
    }
}

#[cfg(test)]
mod tests;
