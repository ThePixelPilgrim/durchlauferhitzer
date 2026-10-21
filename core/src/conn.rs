//! Per-connection state owned by the core.

use std::net::IpAddr;

/// Capacity of each connection's send buffer.
pub(crate) const SEND_CAPACITY: usize = 1024;

/// Where a connection is in its lifecycle. Each variant carries only the data
/// valid in that state; later tasks add variants and fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    /// Reading the request head; `buffered` bytes of `Conn::recv` are filled.
    ReadingHead { buffered: usize },
}

/// One open connection. Dropping it frees both buffers.
#[derive(Debug)]
pub(crate) struct Conn {
    pub(crate) peer: IpAddr,
    pub(crate) state: ConnState,
    /// Fixed-size receive buffer, allocated once on accept.
    pub(crate) recv: Box<[u8]>,
    /// Bytes waiting to be written, at most `SEND_CAPACITY`.
    pub(crate) send: Vec<u8>,
}

impl Conn {
    pub(crate) fn new(peer: IpAddr, recv_capacity: usize) -> Conn {
        Conn {
            peer,
            state: ConnState::ReadingHead { buffered: 0 },
            recv: vec![0; recv_capacity].into_boxed_slice(),
            send: Vec::with_capacity(SEND_CAPACITY),
        }
    }

    /// Spare space the driver may read into; empty when not reading or full.
    pub(crate) fn recv_spare(&mut self) -> &mut [u8] {
        match self.state {
            ConnState::ReadingHead { buffered } => {
                self.recv.get_mut(buffered..).unwrap_or_default()
            }
        }
    }
}
