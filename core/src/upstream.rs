//! Upstreams as the core sees them: backend and upstream ids, which
//! connection each open upstream serves (or whether it is idle in the
//! pool, FR-PROXY-02), and the fixed-size buffers of a proxied exchange. The core never touches a backend socket; the driver
//! connects, reads and writes on its behalf (see the contract in
//! `core/contract.rs`).

mod pool;

use std::collections::BTreeMap;

use crate::core::{ConnId, Interest};
use crate::time::Instant;
use pool::Pool;

/// Opaque upstream id, allocated by the core (sequentially from 1) and
/// never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UpstreamId(pub u64);

/// Index into [`crate::Core::backends`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendId(pub u32);

/// The least capacity of an upstream's receive buffer (32 KiB; owner
/// decision 2026-11-07, NFR-PERF-05: 16 KiB until stage 7). The buffer
/// holds a whole response head, so it is never smaller than `max_head`
/// (see [`recv_capacity`]); body bytes are relayed straight out of it,
/// fill, drain, refill (NFR-RES-04), so it is also the largest piece of a
/// body in flight per exchange. Well above the 35 bytes NFR-RES-04
/// requires.
pub(crate) const UPSTREAM_RECV_MIN: usize = 32 * 1024;

// NFR-RES-04: every body buffer holds more than a chunked-framing tail.
const _: () = assert!(UPSTREAM_RECV_MIN > 35);

/// What re-rendering a head may add beyond one byte per header line:
/// toward the backend `x-forwarded-for` with the peer address (at most 58
/// bytes more), `x-forwarded-proto: http` (25) and `connection: close`
/// (19), 102 in all; toward the client `connection: close` (19), the
/// core's `date` line for a backend that sent none (37, FR-HTTP-22a) and
/// a standard reason in place of an unprintable one (at most 31), 87 in
/// all. Every header line may grow by one byte (`name:value` becomes
/// `name: value`).
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
    /// core's head capacity (checked before every render). Its first
    /// `head_sent` bytes are written; they stay, so a request head can be
    /// replayed on a fresh connection (FR-PROXY-26's retry).
    pub(crate) head: Vec<u8>,
    head_sent: usize,
}

impl ExchangeBuffers {
    pub(crate) fn new(recv_capacity: usize, head_capacity: usize) -> ExchangeBuffers {
        ExchangeBuffers {
            recv: vec![0; recv_capacity].into_boxed_slice(),
            head: Vec::with_capacity(head_capacity),
            head_sent: 0,
        }
    }

    /// The head's bytes not yet written.
    pub(crate) fn head_unsent(&self) -> &[u8] {
        self.head.get(self.head_sent..).unwrap_or_default()
    }

    /// Whether head bytes wait to be written.
    pub(crate) fn head_pending(&self) -> bool {
        self.head_sent < self.head.len()
    }

    /// `len` more head bytes were written (clamped); returns how many.
    pub(crate) fn head_written(&mut self, len: usize) -> usize {
        let sent = len.min(self.head.len().saturating_sub(self.head_sent));
        self.head_sent = self.head_sent.saturating_add(sent);
        sent
    }

    /// Write the whole head again (a replay on a fresh connection).
    pub(crate) fn rewind_head(&mut self) {
        self.head_sent = 0;
    }

    /// Empty the head buffer for the next head to render into it.
    pub(crate) fn clear_head(&mut self) {
        self.head.clear();
        self.head_sent = 0;
    }
}

/// Who holds an open upstream (for drivers' invariant checks): the
/// connection whose exchange it serves, or the idle pool (FR-PROXY-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamHolder {
    Conn(ConnId),
    Pool,
}

/// An open upstream: who holds it and the interest last sent to the
/// driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    holder: UpstreamHolder,
    interest: Interest,
}

/// The core's upstream ids: allocation, who holds each open one (a
/// connection, or the idle pool), and the pool itself. An id is in the
/// table from its `Connect` until its `CloseUpstream`, so removing it is
/// what makes that close happen exactly once.
#[derive(Debug)]
pub(crate) struct UpstreamTable {
    next: u64,
    open: BTreeMap<UpstreamId, Link>,
    pool: Pool,
}

impl UpstreamTable {
    pub(crate) fn new() -> UpstreamTable {
        UpstreamTable {
            next: 1,
            open: BTreeMap::new(),
            pool: Pool::new(),
        }
    }

    /// A fresh id for an upstream that serves `conn`, with interest `None`.
    pub(crate) fn allocate(&mut self, conn: ConnId) -> UpstreamId {
        let up = UpstreamId(self.next);
        self.next = self.next.saturating_add(1);
        self.open.insert(
            up,
            Link {
                holder: UpstreamHolder::Conn(conn),
                interest: Interest::None,
            },
        );
        up
    }

    /// The connection `up` serves, if it is open and not idle.
    pub(crate) fn conn(&self, up: UpstreamId) -> Option<ConnId> {
        match self.open.get(&up)?.holder {
            UpstreamHolder::Conn(conn) => Some(conn),
            UpstreamHolder::Pool => None,
        }
    }

    /// Who holds `up`, if it is open.
    pub(crate) fn holder(&self, up: UpstreamId) -> Option<UpstreamHolder> {
        self.open.get(&up).map(|link| link.holder)
    }

    /// Whether `up` is open (held by a connection or idle).
    pub(crate) fn is_open(&self, up: UpstreamId) -> bool {
        self.open.contains_key(&up)
    }

    /// Whether `up` is idle in the pool.
    pub(crate) fn is_idle(&self, up: UpstreamId) -> bool {
        self.holder(up) == Some(UpstreamHolder::Pool)
    }

    /// Forget `up`, idle or not; whether it was open (and so needs its
    /// `CloseUpstream`).
    pub(crate) fn close(&mut self, up: UpstreamId) -> bool {
        self.pool.remove(up);
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

    /// Park `up`, held by a connection, in the idle pool of `backend`
    /// until `deadline`. With more than `cap` idle for that backend the
    /// oldest of them is returned, out of the pool but still open: the
    /// caller closes it.
    pub(crate) fn park(
        &mut self,
        up: UpstreamId,
        backend: BackendId,
        deadline: Instant,
        cap: usize,
    ) -> Option<UpstreamId> {
        let link = self.open.get_mut(&up)?;
        link.holder = UpstreamHolder::Pool;
        self.pool.park(up, backend, deadline, cap)
    }

    /// The newest idle upstream of `backend`, now held by `conn`.
    pub(crate) fn unpark(&mut self, backend: BackendId, conn: ConnId) -> Option<UpstreamId> {
        let up = self.pool.take(backend)?;
        if let Some(link) = self.open.get_mut(&up) {
            link.holder = UpstreamHolder::Conn(conn);
        }
        Some(up)
    }

    /// The idle upstream that has been idle longest (the earliest idle
    /// deadline), if any.
    pub(crate) fn oldest_idle(&self) -> Option<UpstreamId> {
        self.pool.oldest()
    }

    /// The earliest idle deadline.
    pub(crate) fn next_idle_deadline(&self) -> Option<Instant> {
        self.pool.next_deadline()
    }

    /// The idle upstreams whose deadline is at or before `now`, oldest
    /// first.
    pub(crate) fn idle_due(&self, now: Instant) -> Vec<UpstreamId> {
        self.pool.due(now)
    }

    /// Every idle upstream, oldest first.
    pub(crate) fn idle(&self) -> Vec<UpstreamId> {
        self.pool.all()
    }

    /// Whether no upstream is open.
    pub(crate) fn is_empty(&self) -> bool {
        self.open.is_empty()
    }

    /// How many upstreams are open, idle ones included.
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
