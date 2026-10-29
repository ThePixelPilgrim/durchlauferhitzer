//! A simulated client of the static net, as the test sees it.

use super::*;

/// The test's view of one simulated client.
pub(crate) struct Client {
    pub(crate) id: ConnId,
    /// Templates still to send; the front one may be partly sent.
    pub(crate) queue: VecDeque<Vec<u8>>,
    /// Whether the front template's first byte has been sent.
    pub(crate) front_started: bool,
    /// Templates whose first byte was sent, and how many were `HEAD`.
    pub(crate) started: usize,
    pub(crate) started_head: usize,
    /// Every template whose first byte was sent, in order.
    pub(crate) templates: Vec<Vec<u8>>,
    /// Sends that carried the end of one template and the start of the next.
    pub(crate) pipelined_sends: usize,
    /// Bytes sent so far (of `templates`, in order).
    pub(crate) sent: usize,
    pub(crate) eof: bool,
    pub(crate) reset: ClientReset,
    /// Everything received so far.
    pub(crate) received: Vec<u8>,
    /// The sim reported the connection as no longer `Open`.
    pub(crate) ended: bool,
    pub(crate) role: Role,
}

/// Whether the client reset its connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientReset {
    No,
    Yes,
}

/// What a client is for: a random one, or a witness the drain finds in a
/// known state (`run.rs`), whose bytes are known exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Random,
    /// A `404` queued keep-alive and unread, a second request pipelined
    /// behind it in the same read.
    PipelinedWitness,
    /// A `404` queued keep-alive and unread, nothing behind it.
    InFlightWitness,
    /// Idle keep-alive, its `404` read.
    IdleWitness,
}

impl Client {
    pub(crate) fn new(id: ConnId, role: Role) -> Client {
        Client {
            id,
            queue: VecDeque::new(),
            front_started: false,
            started: 0,
            started_head: 0,
            templates: Vec::new(),
            pipelined_sends: 0,
            sent: 0,
            eof: false,
            reset: ClientReset::No,
            received: Vec::new(),
            ended: false,
            role,
        }
    }
}
