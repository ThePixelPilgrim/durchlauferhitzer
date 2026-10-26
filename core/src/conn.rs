//! Per-connection state owned by the core.

use std::net::IpAddr;

use std::time::Duration;

use crate::config::OnFileChange;
use crate::core::Interest;
use crate::file::{Chunk, FileId, FileStamp, RootId};
use crate::http::request::{HeadParser, Persistence};
use crate::http::response::BodyMode;
use crate::http::static_path::TrailingSlash;
use crate::log::RequestSummary;
use crate::time::Instant;

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

/// What a response leaves for the connection's next request: whether the
/// connection is kept, and how many bytes of pipelined input wait at the
/// front of `Conn::recv`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exchange {
    pub(crate) persistence: Persistence,
    pub(crate) buffered: usize,
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
/// valid in that state, including the instant at which it times out. A
/// state owns its data, so it is not `Copy`: a transition that needs the
/// data by value takes the state with [`Conn::take_state`].
#[derive(Debug)]
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
    /// A response is being written. The core does not read meanwhile: the
    /// `exchange`'s buffered bytes wait for the next request. `deadline` is
    /// the send idle deadline, reset whenever the socket accepts bytes.
    Responding {
        exchange: Exchange,
        unread_body: UnreadBody,
        deadline: Instant,
    },
    /// A body-less GET or HEAD on a static route: `OpenFile` for `file` is
    /// in flight, for the requested path or its directory's index (`step`).
    /// `slash` says whether the path ended in `/`; `body` is `HeadOnly` for
    /// HEAD; `exchange` as in `Responding`. `request` is the request being
    /// answered. `deadline` is the send idle deadline.
    OpeningFile {
        file: FileId,
        slash: TrailingSlash,
        step: OpenStep,
        body: BodyMode,
        exchange: Exchange,
        request: StaticRequest,
        deadline: Instant,
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
        exchange: Exchange,
        deadline: Instant,
    },
    /// The sending side is shut down; input is read and discarded until the
    /// peer closes or `deadline` passes.
    Lingering { deadline: Instant },
}

impl ConnState {
    /// Writing a response that is already rendered into the send buffer,
    /// with no request body left to discard.
    pub(crate) fn responding(exchange: Exchange, deadline: Instant) -> Self {
        ConnState::Responding {
            exchange,
            unread_body: UnreadBody::None,
            deadline,
        }
    }

    /// The instant at which this state times out.
    pub(crate) fn deadline(&self) -> Instant {
        match *self {
            ConnState::ReadingHead { deadline, .. }
            | ConnState::Idle { deadline }
            | ConnState::Responding { deadline, .. }
            | ConnState::OpeningFile { deadline, .. }
            | ConnState::SendingFile { deadline, .. }
            | ConnState::Lingering { deadline } => deadline,
        }
    }

    /// Reset the deadline of a state that responds or waits on the disk;
    /// other states keep theirs.
    pub(crate) fn set_deadline(&mut self, new: Instant) {
        match self {
            ConnState::Responding { deadline, .. }
            | ConnState::OpeningFile { deadline, .. }
            | ConnState::SendingFile { deadline, .. } => *deadline = new,
            ConnState::ReadingHead { .. }
            | ConnState::Idle { .. }
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
            | ConnState::Lingering { .. } => None,
        }
    }
}

/// A request being answered from a static root, kept in `OpeningFile`
/// while its file is opened.
#[derive(Debug)]
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

    /// Take the state out by value, for a transition that needs its data.
    /// A placeholder (`Lingering`, timing out at time zero) stands in, and
    /// the caller replaces it with the new state before it returns.
    pub(crate) fn take_state(&mut self) -> ConnState {
        let placeholder = ConnState::Lingering {
            deadline: Instant::from_nanos(0),
        };
        std::mem::replace(&mut self.state, placeholder)
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
            | ConnState::Lingering { .. } => &mut [],
        }
    }

    /// The interest the connection's state calls for.
    pub(crate) fn wanted_interest(&self) -> Interest {
        if !self.send.is_empty() {
            return Interest::Write;
        }
        match self.state {
            ConnState::ReadingHead { buffered, .. } => {
                if buffered >= self.recv.len() {
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
        }
    }
}
