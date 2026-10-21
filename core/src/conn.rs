//! Per-connection state owned by the core.

use std::net::IpAddr;

use std::time::Duration;

use crate::core::Interest;
use crate::http::request::Persistence;
use crate::time::Instant;

/// Capacity of each connection's send buffer.
pub(crate) const SEND_CAPACITY: usize = 1024;

/// How long a lingering close keeps reading and discarding input before the
/// socket is closed. Fixed on entry into `Lingering`; input does not extend it.
pub(crate) const LINGER_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a connection counts toward the connection limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counted {
    Yes,
    /// Refused with 503: kept only to deliver the answer and linger.
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

/// Where a connection is in its lifecycle. Each variant carries only the data
/// valid in that state, including the instant at which it times out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    /// Reading the request head; `buffered` bytes of `Conn::recv` are filled
    /// and the first `scanned` of them have been searched for an LF.
    /// `deadline` is the total header deadline, set on entry; arriving bytes
    /// do not extend it.
    ReadingHead {
        buffered: usize,
        scanned: usize,
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
    /// The sending side is shut down; input is read and discarded until the
    /// peer closes or `deadline` passes.
    Lingering { deadline: Instant },
}

impl ConnState {
    /// The instant at which this state times out.
    pub(crate) fn deadline(&self) -> Instant {
        match *self {
            ConnState::ReadingHead { deadline, .. }
            | ConnState::Idle { deadline }
            | ConnState::Responding { deadline, .. }
            | ConnState::Lingering { deadline } => deadline,
        }
    }
}

/// One open connection. Dropping it frees both buffers.
#[derive(Debug)]
pub(crate) struct Conn {
    pub(crate) peer: IpAddr,
    pub(crate) counted: Counted,
    pub(crate) state: ConnState,
    /// The interest last sent to the driver.
    pub(crate) interest: Interest,
    /// Fixed-size receive buffer, allocated once on accept.
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

    /// Spare space the driver may read into; empty when not reading or full.
    pub(crate) fn recv_spare(&mut self) -> &mut [u8] {
        match self.state {
            ConnState::ReadingHead { buffered, .. } => {
                self.recv.get_mut(buffered..).unwrap_or_default()
            }
            ConnState::Idle { .. } | ConnState::Lingering { .. } => &mut self.recv,
            ConnState::Responding { .. } => &mut [],
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
        }
    }
}
