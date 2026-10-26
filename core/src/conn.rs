//! Per-connection state owned by the core.

use std::net::IpAddr;

use std::time::Duration;

use crate::config::OnFileChange;
use crate::core::Interest;
use crate::file::{Chunk, FileId, FileStamp, RootId};
use crate::http::request::{HeadParser, Persistence};
use crate::http::response::BodyMode;
use crate::http::response_head::{RequestKind, ResponseParser};
use crate::http::static_path::TrailingSlash;
use crate::log::RequestSummary;
use crate::time::Instant;
use crate::upstream::{BackendId, ExchangeBuffers, UpstreamId};

/// Capacity of each connection's send buffer.
pub(crate) const SEND_CAPACITY: usize = 1024;

/// How long a lingering close keeps reading and discarding input before the
/// socket is closed. Fixed on entry into `Lingering`; input does not extend it.
pub(crate) const LINGER_TIMEOUT: Duration = Duration::from_secs(2);

/// Size of the one scratch buffer the core shares among all lingering
/// connections: they discard their input into it, so none keeps its own
/// receive buffer.
pub(crate) const LINGER_SCRATCH: usize = 4096;

/// Whether a connection counts toward the connection limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    Yes,
    /// Refused with 503: kept only to deliver the answer and linger. It has
    /// no receive buffer of its own.
    No,
}

/// Whether the request's body is still unread on the connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnreadBody {
    None,
    /// The request announced a body the core does not read; the connection
    /// cannot be reused and closes after the response.
    Pending,
}

/// Which open of a static request is in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenStep {
    /// The requested path itself.
    Requested,
    /// The `index.html` of the requested directory (path ended in `/`).
    Index,
}

/// Where a connection is in its lifecycle. Each variant carries only the data
/// valid in that state, including the instant at which it times out. Not
/// `Copy`: a state may own data (the request of a static open) that moves
/// on to the next state or into the log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConnState {
    /// Reading the request head; `buffered` bytes of `Conn::recv` are filled
    /// and `parser` remembers how far it has judged them, so no byte is
    /// parsed again when more arrive. `deadline` is the total header
    /// deadline, set on entry; arriving bytes do not extend it.
    ReadingHead {
        buffered: usize,
        parser: HeadParser,
        deadline: Instant,
    },
    /// Keep-alive between requests, nothing buffered.
    Idle { deadline: Instant },
    /// A response is being written. The first `buffered` bytes of
    /// `Conn::recv` are pipelined input waiting for the next request; the
    /// core does not read meanwhile. `deadline` is the send idle deadline,
    /// reset whenever the socket accepts bytes.
    Responding {
        persistence: Persistence,
        unread_body: UnreadBody,
        buffered: usize,
        deadline: Instant,
    },
    /// A body-less GET or HEAD on a static route: `OpenFile` for `file` is
    /// in flight, for the requested path or its directory's index (`step`).
    /// `slash` says whether the path ended in `/`; `body` is `HeadOnly` for
    /// HEAD; `persistence` and `buffered` as in `Responding`. `deadline` is
    /// the send idle deadline. `request` is the request being answered.
    OpeningFile {
        file: FileId,
        slash: TrailingSlash,
        step: OpenStep,
        body: BodyMode,
        persistence: Persistence,
        buffered: usize,
        deadline: Instant,
        request: StaticRequest,
    },
    /// A file response: the head is in `Conn::send`, then the open `file`'s
    /// bytes from `offset` up to `size` (its size at open) follow chunk by
    /// chunk. Every chunk's stamp is checked against `opened` (the stamp at
    /// open) under the route's `on_change`. `deadline` is the send idle
    /// deadline, reset on progress (of the socket or the disk).
    SendingFile {
        file: FileId,
        size: u64,
        opened: FileStamp,
        on_change: OnFileChange,
        offset: u64,
        chunk: Chunk,
        persistence: Persistence,
        buffered: usize,
        deadline: Instant,
    },
    /// A body-less request on a proxy route: the `Connect` for `up` is in
    /// flight and the rewritten request head waits in `buffers.head`.
    /// `deadline` is the backend connect timeout.
    Connecting {
        up: UpstreamId,
        request: ProxyRequest,
        buffers: ExchangeBuffers,
        deadline: Instant,
    },
    /// Connected to `up`: the request head is written from `buffers.head`
    /// while it holds bytes, then the response head is read into
    /// `buffers.recv`, whose first `filled` bytes `parser` has judged so far.
    /// `deadline` is the backend response timeout, restarted once the
    /// request head is fully written.
    AwaitingResponse {
        up: UpstreamId,
        request: ProxyRequest,
        buffers: ExchangeBuffers,
        filled: usize,
        parser: ResponseParser,
        deadline: Instant,
    },
    /// The backend's response head was queued: what is left of its rewrite
    /// is in `buffers.head`, then the body bytes `buffers.recv[start..end]`
    /// go out, straight from where the backend's bytes landed. `source` says
    /// whether more body bytes are due from the upstream. `persistence` and
    /// `buffered` as in `Responding`. `deadline` is the send idle timeout
    /// while the client has bytes to take or nothing more is due, the body
    /// idle timeout while it waits for the backend's next bytes; reset on
    /// progress.
    Relaying {
        source: Source,
        buffers: ExchangeBuffers,
        start: usize,
        end: usize,
        persistence: Persistence,
        buffered: usize,
        deadline: Instant,
    },
    /// The sending side is shut down; input is read and discarded until the
    /// peer closes or `deadline` passes.
    Lingering { deadline: Instant },
}

/// Where the body of a relayed response comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// `remaining` (more than 0) body bytes are still due from upstream
    /// `up` of `backend`.
    Open {
        up: UpstreamId,
        backend: BackendId,
        remaining: u64,
    },
    /// Every body byte arrived (or there is no body); the upstream is
    /// closed.
    Done,
}

/// A request being forwarded to a backend, kept until its response head
/// (or the failure that replaces it) is logged.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ProxyRequest {
    pub(crate) summary: RequestSummary,
    pub(crate) backend: BackendId,
    /// A response to HEAD never has a body.
    pub(crate) kind: RequestKind,
    pub(crate) persistence: Persistence,
    /// Pipelined input waiting at the front of `Conn::recv`.
    pub(crate) buffered: usize,
}

impl ConnState {
    /// The instant at which this state times out.
    pub(crate) fn deadline(&self) -> Instant {
        match *self {
            ConnState::ReadingHead { deadline, .. }
            | ConnState::Idle { deadline }
            | ConnState::Responding { deadline, .. }
            | ConnState::OpeningFile { deadline, .. }
            | ConnState::SendingFile { deadline, .. }
            | ConnState::Connecting { deadline, .. }
            | ConnState::AwaitingResponse { deadline, .. }
            | ConnState::Relaying { deadline, .. }
            | ConnState::Lingering { deadline } => deadline,
        }
    }

    /// Reset the deadline of a state that responds or waits on the disk;
    /// other states keep theirs (proxy states set theirs themselves).
    pub(crate) fn set_deadline(&mut self, new: Instant) {
        match self {
            ConnState::Responding { deadline, .. }
            | ConnState::OpeningFile { deadline, .. }
            | ConnState::SendingFile { deadline, .. } => *deadline = new,
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => {}
        }
    }

    /// The file this state holds an id for, if any.
    pub(crate) fn file(&self) -> Option<FileId> {
        match *self {
            ConnState::OpeningFile { file, .. } | ConnState::SendingFile { file, .. } => Some(file),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => None,
        }
    }

    /// The open upstream this state holds an id for, if any.
    pub(crate) fn upstream(&self) -> Option<UpstreamId> {
        match *self {
            ConnState::Connecting { up, .. }
            | ConnState::AwaitingResponse { up, .. }
            | ConnState::Relaying {
                source: Source::Open { up, .. },
                ..
            } => Some(up),
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Relaying {
                source: Source::Done,
                ..
            }
            | ConnState::Lingering { .. } => None,
        }
    }

    /// The open upstream of this state and the interest it calls for:
    /// nothing while connecting; writing the request head, then reading
    /// the response head; reading body bytes only once every earlier one
    /// was sent (fill, drain, refill).
    pub(crate) fn upstream_interest(&self) -> Option<(UpstreamId, Interest)> {
        match self {
            ConnState::Connecting { up, .. } => Some((*up, Interest::None)),
            ConnState::AwaitingResponse {
                up,
                buffers,
                filled,
                ..
            } => {
                let interest = if !buffers.head.is_empty() {
                    Interest::Write
                } else if *filled < buffers.recv.len() {
                    Interest::Read
                } else {
                    Interest::None
                };
                Some((*up, interest))
            }
            ConnState::Relaying {
                source: Source::Open { up, .. },
                start,
                end,
                ..
            } => {
                let interest = if start >= end {
                    Interest::Read
                } else {
                    Interest::None
                };
                Some((*up, interest))
            }
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
            | ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Relaying {
                source: Source::Done,
                ..
            }
            | ConnState::Lingering { .. } => None,
        }
    }
}

/// A request being answered from a static root, kept while its file is
/// opened.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct StaticRequest {
    /// Logged once the outcome of the open is known.
    pub(crate) summary: RequestSummary,
    /// The raw query, kept for a redirect.
    pub(crate) query: Option<String>,
    pub(crate) root: RootId,
    /// The route's policy for a file that changes while it is sent.
    pub(crate) on_change: OnFileChange,
    /// The relative segments of the path being opened.
    pub(crate) path: Vec<Vec<u8>>,
}

/// One open connection. Dropping it frees both buffers.
#[derive(Debug)]
pub(crate) struct Conn {
    pub(crate) peer: IpAddr,
    pub(crate) counted: Counted,
    pub(crate) state: ConnState,
    /// The interest last sent to the driver.
    pub(crate) interest: Interest,
    /// Fixed-size receive buffer, allocated once on accept (empty for a
    /// refused connection) and dropped on entry into `Lingering`.
    pub(crate) recv: Box<[u8]>,
    /// Bytes waiting to be written, at most `SEND_CAPACITY`.
    pub(crate) send: Vec<u8>,
}

impl Conn {
    pub(crate) fn new(
        peer: IpAddr,
        counted: Counted,
        state: ConnState,
        recv_capacity: usize,
    ) -> Conn {
        Conn {
            peer,
            counted,
            state,
            interest: Interest::None,
            recv: vec![0; recv_capacity].into_boxed_slice(),
            send: Vec::with_capacity(SEND_CAPACITY),
        }
    }

    /// Spare space of the connection's own buffer the driver may read into;
    /// empty when not reading or full. A lingering connection has no buffer
    /// of its own: it reads into the core's shared scratch buffer instead.
    pub(crate) fn recv_spare(&mut self) -> &mut [u8] {
        match self.state {
            ConnState::ReadingHead { buffered, .. } => {
                self.recv.get_mut(buffered..).unwrap_or_default()
            }
            ConnState::Idle { .. } => &mut self.recv,
            ConnState::Responding { .. }
            | ConnState::OpeningFile { .. }
            | ConnState::SendingFile { .. }
            | ConnState::Connecting { .. }
            | ConnState::AwaitingResponse { .. }
            | ConnState::Relaying { .. }
            | ConnState::Lingering { .. } => &mut [],
        }
    }

    /// Take the state out so its owned data can move on; the caller sets
    /// the next state. Until then a placeholder with the same deadline
    /// holds the place.
    pub(crate) fn take_state(&mut self) -> ConnState {
        let placeholder = ConnState::Lingering {
            deadline: self.state.deadline(),
        };
        std::mem::replace(&mut self.state, placeholder)
    }

    /// The interest the connection's state calls for.
    pub(crate) fn wanted_interest(&self) -> Interest {
        if !self.send.is_empty() {
            return Interest::Write;
        }
        match &self.state {
            ConnState::ReadingHead { buffered, .. } => {
                if *buffered >= self.recv.len() {
                    Interest::None
                } else {
                    Interest::Read
                }
            }
            ConnState::Idle { .. } | ConnState::Lingering { .. } => Interest::Read,
            // A response in progress always has bytes to send; reaching this
            // arm means nothing is left to do but wait.
            ConnState::Responding { .. } => Interest::None,
            // Waiting on the disk.
            ConnState::OpeningFile { .. } => Interest::None,
            ConnState::SendingFile { chunk, .. } => match chunk {
                Chunk::Ready { .. } => Interest::Write,
                Chunk::Reading { .. } | Chunk::Done => Interest::None,
            },
            // Waiting on the backend.
            ConnState::Connecting { .. } | ConnState::AwaitingResponse { .. } => Interest::None,
            ConnState::Relaying {
                buffers,
                start,
                end,
                ..
            } => {
                if !buffers.head.is_empty() || start < end {
                    Interest::Write
                } else {
                    Interest::None
                }
            }
        }
    }
}
