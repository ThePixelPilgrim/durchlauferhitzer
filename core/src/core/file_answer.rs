//! The answer to a request whose regular file is open: `304`, the whole
//! file, one range of it or `416` (FR-STATIC-18, -19), and the
//! connection's state once that answer's head is queued (FR-STATIC-13,
//! -17). Static routes and backend-directed files (FR-PROXY-32) both
//! decide here, so the conditional, range and `HEAD` rules exist once.

use super::{Action, ConnId};
use crate::config::OnFileChange;
use crate::conn::{BackendFile, Conn, ConnState, FileHead, FileSource, StaticRequest, UnreadBody};
use crate::file::{Chunk, FILE_CHUNK, FileId, FileStamp, FileTable};
use crate::http::conditional::{Freshness, Validators};
use crate::http::date::ImfFixdate;
use crate::http::range::Ranged;
use crate::http::request::{Method, Persistence};
use crate::http::response::{
    BodyMode, FileStatus, Part, render_backend_file_head, render_file_head, render_not_modified,
    render_range_not_satisfiable,
};
use crate::http::status::Status;
use crate::log::Log;
use crate::time::Instant;
use crate::upstream::BackendId;

/// How an open regular file answers its request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FileAnswer {
    /// The conditional headers say the client has it (`304`).
    NotModified,
    /// The file's head, then (for a body) its bytes: whole or one range.
    File(Part),
    /// The range does not overlap the file (`416`); the file is not read.
    Unsatisfiable,
}

impl FileAnswer {
    /// The answer to `request` for a file of `stamp` (whose `validators`
    /// they are), the driver's clock at `clock`: the conditional headers
    /// first (FR-STATIC-18, judged on the stamp of the opened descriptor,
    /// so a file changed since the client's copy is sent anew), then the
    /// range (FR-STATIC-19, a `GET`'s only). The conditional headers count
    /// for a `GET` or `HEAD` only (RFC 9110 section 13.1.2: a `304` answers
    /// nothing else); a backend-directed file may answer any method.
    pub(super) fn of(
        request: &StaticRequest,
        validators: &Validators,
        stamp: FileStamp,
        clock: Option<u64>,
    ) -> FileAnswer {
        let freshness = match request.summary.method {
            Method::Get | Method::Head => request.conditional.not_modified(stamp, clock),
            Method::Other(_) => Freshness::Modified,
        };
        match freshness {
            Freshness::NotModified => FileAnswer::NotModified,
            Freshness::Modified => {
                let size = stamp.size;
                match request
                    .range
                    .judge(&request.summary.method, validators, size)
                {
                    Ranged::Whole => FileAnswer::File(Part::Whole { size }),
                    Ranged::Part { first, last } => {
                        FileAnswer::File(Part::Range { first, last, size })
                    }
                    Ranged::Unsatisfiable => FileAnswer::Unsatisfiable,
                }
            }
        }
    }

    /// The status the answer's head carries.
    pub(super) fn status(self) -> Status {
        match self {
            FileAnswer::NotModified => Status::NotModified,
            FileAnswer::File(Part::Whole { .. }) => Status::Ok,
            FileAnswer::File(Part::Range { .. }) => Status::PartialContent,
            FileAnswer::Unsatisfiable => Status::RangeNotSatisfiable,
        }
    }
}

/// What an answer's head is rendered from, besides its request.
pub(super) struct Rendering<'a> {
    /// The open file's validators and size.
    pub(super) validators: &'a Validators,
    pub(super) size: u64,
    pub(super) body: BodyMode,
    pub(super) persistence: Persistence,
    /// The core's `Date` value, if the driver set one.
    pub(super) date: Option<&'a ImfFixdate>,
}

/// Queue the head of `answer` to `request` on `c` and log it once. A
/// static route's heads (FR-STATIC-11, -18, -19) go to `Conn::send`,
/// logged as [`Log::Response`]. A backend-directed file's `200`, `206` or
/// `304` is its merged head (FR-PROXY-32), rendered in the exchange's head
/// buffer it kept, which becomes the head of its own, logged as
/// [`Log::BackendFile`]; its `416` is the plain one, the backend's headers
/// dropped, logged as a `Response`. Returns where the head waits.
pub(super) fn queue_head(
    c: &mut Conn,
    conn: ConnId,
    request: StaticRequest,
    answer: FileAnswer,
    with: &Rendering<'_>,
    actions: &mut Vec<Action>,
) -> FileHead {
    let Rendering {
        validators,
        size,
        body,
        persistence,
        date,
    } = *with;
    let content_type = request.content_type;
    let status = answer.status();
    let (log, head) = match (request.source, answer) {
        (FileSource::Route { .. } | FileSource::Backend(_), FileAnswer::Unsatisfiable) => {
            render_range_not_satisfiable(size, persistence, body, date, &mut c.send);
            (None, FileHead::SendBuffer)
        }
        (FileSource::Route { .. }, FileAnswer::NotModified) => {
            render_not_modified(validators, persistence, date, &mut c.send);
            (None, FileHead::SendBuffer)
        }
        (FileSource::Route { .. }, FileAnswer::File(part)) => {
            render_file_head(
                content_type,
                part,
                validators,
                persistence,
                date,
                &mut c.send,
            );
            (None, FileHead::SendBuffer)
        }
        (FileSource::Backend(file), FileAnswer::NotModified) => {
            merged(file, FileStatus::NotModified, content_type, with)
        }
        (FileSource::Backend(file), FileAnswer::File(part)) => {
            merged(file, FileStatus::File(part), content_type, with)
        }
    };
    actions.push(Action::Log(match log {
        Some(backend) => Log::BackendFile {
            conn,
            peer: c.peer,
            backend,
            status,
            request: request.summary,
        },
        None => Log::Response {
            conn,
            peer: c.peer,
            status,
            request: Some(request.summary),
        },
    }));
    head
}

/// A backend-directed file's merged head, rendered around its kept lines
/// in the buffer that holds them, and the backend to log.
fn merged(
    mut file: BackendFile,
    status: FileStatus,
    content_type: &str,
    with: &Rendering<'_>,
) -> (Option<BackendId>, FileHead) {
    render_backend_file_head(
        status,
        &mut file.head,
        file.kept,
        content_type,
        with.validators,
        with.persistence,
        with.date,
    );
    let head = FileHead::Own {
        bytes: file.head,
        sent: 0,
    };
    (Some(file.backend), head)
}

/// An open file whose answer's head was just queued, and how its
/// connection goes on.
pub(super) struct Queued {
    pub(super) file: FileId,
    pub(super) answer: FileAnswer,
    pub(super) body: BodyMode,
    /// The file's stamp at open.
    pub(super) opened: FileStamp,
    pub(super) on_change: OnFileChange,
    pub(super) persistence: Persistence,
    pub(super) buffered: usize,
    pub(super) deadline: Instant,
    /// Where the head waits: in `Conn::send`, or (a backend-directed
    /// file's `200`, `206` or `304`) of its own. A `416` is always in
    /// `Conn::send`.
    pub(super) head: FileHead,
}

/// The connection's state once `queued`'s head is queued: a file with
/// bytes to send reads its first chunk ahead (FR-STATIC-17); a `HEAD`
/// (never ranged), a `304` and a `416` are the head alone and nothing is
/// read (FR-STATIC-13), so the file is closed at once, or, behind a head
/// of its own, once that head is written (it is sent while the file is
/// held, like a body).
pub(super) fn after_head(
    files: &mut FileTable,
    queued: Queued,
    actions: &mut Vec<Action>,
) -> ConnState {
    let Queued {
        file,
        answer,
        body,
        opened,
        on_change,
        persistence,
        buffered,
        deadline,
        head,
    } = queued;
    let (offset, end) = match (answer, body) {
        (FileAnswer::File(Part::Whole { size }), BodyMode::Full) => (0, size),
        (FileAnswer::File(Part::Range { first, last, .. }), BodyMode::Full) => {
            (first, last.saturating_add(1))
        }
        // Nothing of the file follows the head.
        (FileAnswer::NotModified, BodyMode::Full | BodyMode::HeadOnly)
        | (FileAnswer::File(_), BodyMode::HeadOnly) => match head {
            FileHead::Own { .. } => (0, 0),
            FileHead::SendBuffer => {
                return closed(files, file, persistence, buffered, deadline, actions);
            }
        },
        (FileAnswer::Unsatisfiable, BodyMode::Full | BodyMode::HeadOnly) => {
            return closed(files, file, persistence, buffered, deadline, actions);
        }
    };
    let chunk = if offset >= end {
        Chunk::Done
    } else {
        let len = end.saturating_sub(offset).min(FILE_CHUNK);
        actions.push(Action::Readahead { file, offset, len });
        Chunk::Reading {
            end: offset.saturating_add(len),
        }
    };
    ConnState::SendingFile {
        file,
        end,
        opened,
        on_change,
        offset,
        chunk,
        persistence,
        buffered,
        deadline,
        head,
    }
}

/// Close `file`: the head in `Conn::send` is all the response.
fn closed(
    files: &mut FileTable,
    file: FileId,
    persistence: Persistence,
    buffered: usize,
    deadline: Instant,
    actions: &mut Vec<Action>,
) -> ConnState {
    files.release(file);
    actions.push(Action::CloseFile { file });
    ConnState::Responding {
        persistence,
        unread_body: UnreadBody::None,
        buffered,
        deadline,
    }
}
