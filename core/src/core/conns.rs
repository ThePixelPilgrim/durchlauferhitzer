//! The open connections, with an index of their deadlines (NFR-PERF-06):
//! the earliest deadline and the connections due are found without a scan
//! of every connection.
//!
//! Every connection's state carries its deadline ([`ConnState::deadline`]),
//! and the core changes states in place through [`Conns::get_mut`], the
//! only mutable access. So no change can bypass the index: `get_mut`
//! records the connection as touched, and [`Conns::rekey`] (called by the
//! core before each of its public mutating calls returns, and before each
//! pass over expired connections) moves every touched connection whose
//! deadline changed to its new key, in O(log n). Inserts and removals
//! update the index at once.
//!
//! Memory stays bounded by the connections: one index entry per
//! connection, and a touched list holding each connection at most once
//! between two re-keys (a flag per connection; ids are never reused).
//!
//! In builds with debug assertions (the per-push gate and the sim nets),
//! every query also checks the index against the old scan.

use std::collections::{BTreeMap, BTreeSet};

use super::ConnId;
use crate::conn::Conn;
#[cfg(doc)]
use crate::conn::ConnState;
use crate::time::Instant;

/// Whether a connection was handed out mutably since the last re-key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Touched {
    No,
    Yes,
}

#[derive(Debug)]
struct Slot {
    conn: Conn,
    /// The deadline the index holds for this connection.
    key: Instant,
    touched: Touched,
}

/// Every open connection by id, and their deadlines in order.
#[derive(Debug, Default)]
pub(crate) struct Conns {
    slots: BTreeMap<ConnId, Slot>,
    /// `(key, id)` of every slot.
    by_deadline: BTreeSet<(Instant, ConnId)>,
    /// Slots touched since the last re-key, each once.
    touched: Vec<ConnId>,
}

impl Conns {
    pub(crate) fn new() -> Conns {
        Conns::default()
    }

    pub(crate) fn get(&self, id: &ConnId) -> Option<&Conn> {
        self.slots.get(id).map(|s| &s.conn)
    }

    /// The connection, to change in place; its deadline is re-keyed at
    /// the next [`Conns::rekey`].
    pub(crate) fn get_mut(&mut self, id: &ConnId) -> Option<&mut Conn> {
        let slot = self.slots.get_mut(id)?;
        if slot.touched == Touched::No {
            slot.touched = Touched::Yes;
            self.touched.push(*id);
        }
        Some(&mut slot.conn)
    }

    pub(crate) fn contains_key(&self, id: &ConnId) -> bool {
        self.slots.contains_key(id)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &ConnId> {
        self.slots.keys()
    }

    pub(crate) fn values(&self) -> impl Iterator<Item = &Conn> {
        self.slots.values().map(|s| &s.conn)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Add `conn` under `id` (a new id: the driver never reuses one).
    pub(crate) fn insert(&mut self, id: ConnId, conn: Conn) {
        let key = conn.state.deadline();
        let slot = Slot {
            conn,
            key,
            touched: Touched::No,
        };
        if let Some(old) = self.slots.insert(id, slot) {
            self.by_deadline.remove(&(old.key, id));
        }
        self.by_deadline.insert((key, id));
    }

    pub(crate) fn remove(&mut self, id: &ConnId) -> Option<Conn> {
        let slot = self.slots.remove(id)?;
        self.by_deadline.remove(&(slot.key, *id));
        Some(slot.conn)
    }

    /// Bring the index in step with every connection touched since the
    /// last re-key.
    pub(crate) fn rekey(&mut self) {
        for id in self.touched.drain(..) {
            let Some(slot) = self.slots.get_mut(&id) else {
                continue;
            };
            slot.touched = Touched::No;
            let deadline = slot.conn.state.deadline();
            if deadline != slot.key {
                self.by_deadline.remove(&(slot.key, id));
                self.by_deadline.insert((deadline, id));
                slot.key = deadline;
            }
        }
    }

    /// The earliest deadline of any connection; `None` without any.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.check();
        self.by_deadline.first().map(|&(key, _)| key)
    }

    /// Every connection whose deadline is at or before `now`, in id order.
    pub(crate) fn due(&self, now: Instant) -> Vec<ConnId> {
        self.check();
        let mut due: Vec<ConnId> = self
            .by_deadline
            .iter()
            .take_while(|&&(key, _)| key <= now)
            .map(|&(_, id)| id)
            .collect();
        due.sort_unstable();
        due
    }

    /// With debug assertions: the index holds exactly every connection's
    /// current deadline, so it answers as the old scan does. A deadline
    /// changed without a re-key fails here.
    fn check(&self) {
        if cfg!(debug_assertions) {
            debug_assert_eq!(
                self.by_deadline.len(),
                self.slots.len(),
                "deadline index size"
            );
            for (id, slot) in &self.slots {
                let deadline = slot.conn.state.deadline();
                debug_assert!(
                    deadline == slot.key && self.by_deadline.contains(&(deadline, *id)),
                    "deadline index out of step for {id:?}: indexed {:?}, state {deadline:?}",
                    slot.key
                );
            }
            debug_assert_eq!(
                self.by_deadline.first().map(|&(key, _)| key),
                self.values().map(|c| c.state.deadline()).min(),
                "deadline index differs from the scan"
            );
        }
    }

    /// Index entries (for tests: one per connection).
    #[cfg(test)]
    pub(crate) fn indexed(&self) -> usize {
        self.by_deadline.len()
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;
    use crate::conn::{ConnState, Counted};

    const PEER: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    /// xorshift64*: a fixed, std-only sequence.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn at(nanos: u64) -> Instant {
        Instant::from_nanos(nanos)
    }

    /// An `Idle` or a `Lingering` state with deadline `t`, picked by
    /// `kind`.
    fn state(kind: u64, t: Instant) -> ConnState {
        match kind % 2 {
            0 => ConnState::Idle { deadline: t },
            _ => ConnState::Lingering { deadline: t },
        }
    }

    fn conn(kind: u64, t: Instant) -> Conn {
        Conn::new(PEER, Counted::No, state(kind, t), 0)
    }

    /// The old O(n) scan: the earliest deadline.
    fn scan_min(conns: &Conns) -> Option<Instant> {
        conns.values().map(|c| c.state.deadline()).min()
    }

    /// The old O(n) scan: every connection due at `now`, in id order.
    fn scan_due(conns: &Conns, now: Instant) -> Vec<ConnId> {
        conns
            .keys()
            .copied()
            .filter(|id| conns.get(id).is_some_and(|c| c.state.deadline() <= now))
            .collect()
    }

    #[test]
    fn an_empty_index_has_no_deadline() {
        let conns = Conns::new();
        assert_eq!(conns.next_deadline(), None);
        assert!(conns.due(at(u64::MAX)).is_empty());
    }

    #[test]
    fn the_index_follows_inserts_changes_and_removals() {
        let mut conns = Conns::new();
        conns.insert(ConnId(2), conn(0, at(30)));
        conns.insert(ConnId(1), conn(1, at(30)));
        conns.insert(ConnId(3), conn(0, at(10)));
        assert_eq!(conns.next_deadline(), Some(at(10)));
        assert_eq!(conns.due(at(30)), vec![ConnId(1), ConnId(2), ConnId(3)]);
        if let Some(c) = conns.get_mut(&ConnId(3)) {
            c.state = ConnState::Idle { deadline: at(40) };
        }
        conns.rekey();
        assert_eq!(conns.next_deadline(), Some(at(30)));
        assert_eq!(conns.due(at(30)), vec![ConnId(1), ConnId(2)]);
        assert!(conns.remove(&ConnId(1)).is_some());
        assert!(conns.remove(&ConnId(1)).is_none());
        assert_eq!(conns.due(at(30)), vec![ConnId(2)]);
        assert!(conns.remove(&ConnId(2)).is_some());
        assert_eq!(conns.next_deadline(), Some(at(40)));
        assert_eq!(conns.len(), 1);
    }

    /// Random inserts, deadline changes (through `get_mut`, as the core
    /// makes them), removals and re-keys: after every re-key the index
    /// answers exactly as the old scan, and holds one entry per
    /// connection (bounded by the connections).
    #[test]
    fn the_index_equals_the_scan_under_random_updates() {
        for seed in 1..=200u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut conns = Conns::new();
            let mut next_id = 0u64;
            for _ in 0..300 {
                match rng.below(10) {
                    0..=2 => {
                        conns.insert(ConnId(next_id), conn(rng.next(), at(rng.below(50))));
                        next_id += 1;
                    }
                    3..=6 if next_id > 0 => {
                        let id = ConnId(rng.below(next_id));
                        let (kind, t) = (rng.next(), at(rng.below(50)));
                        // Several changes before one re-key, some to the
                        // same deadline, some only touching the conn.
                        if let Some(c) = conns.get_mut(&id) {
                            match rng.below(3) {
                                0 => {}
                                _ => c.state = state(kind, t),
                            }
                        }
                    }
                    7 if next_id > 0 => {
                        conns.remove(&ConnId(rng.below(next_id)));
                    }
                    _ => {
                        conns.rekey();
                        assert_eq!(conns.next_deadline(), scan_min(&conns), "seed {seed}");
                        let now = at(rng.below(60));
                        assert_eq!(conns.due(now), scan_due(&conns, now), "seed {seed}");
                        assert_eq!(conns.indexed(), conns.len(), "seed {seed}");
                    }
                }
            }
        }
    }
}
