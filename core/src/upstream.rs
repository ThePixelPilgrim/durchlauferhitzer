//! Upstreams as the core sees them: backend and upstream ids, which
//! connection each open upstream serves, and the fixed-size buffers of a
//! proxied exchange. The core never touches a backend socket; the driver
//! connects, reads and writes on its behalf (see the contract in
//! `core/contract.rs`).

use std::collections::BTreeMap;

use crate::core::{ConnId, Interest};

/// Opaque upstream id, allocated by the core (sequentially from 1) and
/// never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UpstreamId(pub u64);

/// Index into [`crate::Core::backends`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendId(pub u32);

/// The least capacity of an upstream's receive buffer (16 KiB). The buffer
/// holds a whole response head, so it is never smaller than `max_head`
/// (see [`recv_capacity`]); body bytes are relayed straight out of it,
/// fill, drain, refill (NFR-RES-04), so it is also the largest piece of a
/// body in flight per exchange. Well above the 35 bytes NFR-RES-04
/// requires.
pub(crate) const UPSTREAM_RECV_MIN: usize = 16 * 1024;

// NFR-RES-04: every body buffer holds more than a chunked-framing tail.
const _: () = assert!(UPSTREAM_RECV_MIN > 35);

/// What re-rendering a head may add beyond one byte per header line:
/// toward the backend `x-forwarded-for` with the peer address (at most 58
/// bytes more), `x-forwarded-proto: http` (25) and `connection: close`
/// (19); toward the client `connection: close` (19) and a standard reason
/// in place of an unprintable one (at most 31). Every header line may grow
/// by one byte (`name:value` becomes `name: value`).
const HEAD_SLACK: usize = 128;

/// Capacity of an upstream's receive buffer for heads of at most
/// `max_head` bytes.
pub(crate) fn recv_capacity(max_head: usize) -> usize {
    UPSTREAM_RECV_MIN.max(max_head)
}

/// Capacity of an exchange's head buffer: room for any rewritten head
/// whose original obeyed `max_head` and `max_headers`.
pub(crate) fn head_capacity(max_head: usize, max_headers: usize) -> usize {
    max_head
        .saturating_add(max_headers)
        .saturating_add(HEAD_SLACK)
}

/// The buffers of one proxied exchange, allocated when it connects and
/// dropped with the connection state that owns them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExchangeBuffers {
    /// What the backend sends: the response head, then body bytes. Its
    /// length is its fixed capacity.
    pub(crate) recv: Box<[u8]>,
    /// A rewritten head on its way out: first the request head toward the
    /// backend, then the response head toward the client. At most the
    /// core's head capacity (checked before every render).
    pub(crate) head: Vec<u8>,
}

impl ExchangeBuffers {
    pub(crate) fn new(recv_capacity: usize, head_capacity: usize) -> ExchangeBuffers {
        ExchangeBuffers {
            recv: vec![0; recv_capacity].into_boxed_slice(),
            head: Vec::with_capacity(head_capacity),
        }
    }
}

/// An open upstream: the connection it serves and the interest last sent
/// to the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    conn: ConnId,
    interest: Interest,
}

/// The core's upstream ids: allocation, and which connection each open one
/// serves. An id is in the table from its `Connect` until its
/// `CloseUpstream`, so removing it is what makes that close happen exactly
/// once.
#[derive(Debug)]
pub(crate) struct UpstreamTable {
    next: u64,
    open: BTreeMap<UpstreamId, Link>,
}

impl UpstreamTable {
    pub(crate) fn new() -> UpstreamTable {
        UpstreamTable {
            next: 1,
            open: BTreeMap::new(),
        }
    }

    /// A fresh id for an upstream that serves `conn`, with interest `None`.
    pub(crate) fn allocate(&mut self, conn: ConnId) -> UpstreamId {
        let up = UpstreamId(self.next);
        self.next = self.next.saturating_add(1);
        self.open.insert(
            up,
            Link {
                conn,
                interest: Interest::None,
            },
        );
        up
    }

    /// The connection `up` serves, if it is open.
    pub(crate) fn conn(&self, up: UpstreamId) -> Option<ConnId> {
        self.open.get(&up).map(|link| link.conn)
    }

    /// Forget `up`; whether it was open (and so needs its `CloseUpstream`).
    pub(crate) fn close(&mut self, up: UpstreamId) -> bool {
        self.open.remove(&up).is_some()
    }

    /// Record `interest` for `up`; whether it is open and the interest
    /// changed (and so needs a `SetUpstreamInterest`).
    pub(crate) fn set_interest(&mut self, up: UpstreamId, interest: Interest) -> bool {
        match self.open.get_mut(&up) {
            Some(link) if link.interest != interest => {
                link.interest = interest;
                true
            }
            Some(_) | None => false,
        }
    }

    /// How many upstreams are open.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.open.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_sequential_from_one_and_closed_once() {
        let mut table = UpstreamTable::new();
        let a = table.allocate(ConnId(7));
        let b = table.allocate(ConnId(8));
        assert_eq!((a, b), (UpstreamId(1), UpstreamId(2)));
        assert_eq!(table.conn(a), Some(ConnId(7)));
        assert!(table.close(a));
        assert!(!table.close(a));
        assert_eq!(table.conn(a), None);
        // Never reused.
        assert_eq!(table.allocate(ConnId(7)), UpstreamId(3));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn interest_changes_are_reported_once() {
        let mut table = UpstreamTable::new();
        let up = table.allocate(ConnId(1));
        assert!(!table.set_interest(up, Interest::None));
        assert!(table.set_interest(up, Interest::Write));
        assert!(!table.set_interest(up, Interest::Write));
        table.close(up);
        assert!(!table.set_interest(up, Interest::Read));
    }

    #[test]
    fn capacities_hold_a_head_and_exceed_the_chunked_tail() {
        assert_eq!(recv_capacity(1024), UPSTREAM_RECV_MIN);
        assert_eq!(recv_capacity(64 * 1024), 64 * 1024);
        assert_eq!(head_capacity(100, 10), 238);
    }
}
