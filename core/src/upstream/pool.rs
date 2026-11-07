//! The idle pool (FR-PROXY-02): upstreams kept open after a complete,
//! reusable response, per backend, the newest reused first (LIFO), at
//! most a cap per backend, each until its idle deadline. An idle upstream
//! holds no buffer; the pool holds only its id, backend and deadline.

use std::collections::{BTreeMap, BTreeSet};

use super::{BackendId, UpstreamId};
use crate::time::Instant;

#[derive(Debug, Default)]
pub(super) struct Pool {
    /// Per backend, its idle upstreams oldest first (the last is reused
    /// first). No entry is empty.
    stacks: BTreeMap<BackendId, Vec<UpstreamId>>,
    /// Every idle upstream by its deadline: the oldest first.
    by_deadline: BTreeSet<(Instant, UpstreamId)>,
    /// Each idle upstream's backend and deadline.
    entries: BTreeMap<UpstreamId, (BackendId, Instant)>,
}

impl Pool {
    pub(super) fn new() -> Pool {
        Pool::default()
    }

    /// Add `up` of `backend` as the newest idle one, until `deadline`.
    /// With more than `cap` idle for `backend`, the oldest of them leaves
    /// the pool and is returned.
    pub(super) fn park(
        &mut self,
        up: UpstreamId,
        backend: BackendId,
        deadline: Instant,
        cap: usize,
    ) -> Option<UpstreamId> {
        self.remove(up);
        self.stacks.entry(backend).or_default().push(up);
        self.by_deadline.insert((deadline, up));
        self.entries.insert(up, (backend, deadline));
        let over = self
            .stacks
            .get(&backend)
            .is_some_and(|stack| stack.len() > cap);
        let oldest = over
            .then(|| self.stacks.get(&backend).and_then(|s| s.first().copied()))
            .flatten()?;
        self.remove(oldest);
        Some(oldest)
    }

    /// Take the newest idle upstream of `backend`.
    pub(super) fn take(&mut self, backend: BackendId) -> Option<UpstreamId> {
        let up = self.stacks.get(&backend)?.last().copied()?;
        self.remove(up);
        Some(up)
    }

    /// Take `up` out of the pool; whether it was idle.
    pub(super) fn remove(&mut self, up: UpstreamId) -> bool {
        let Some((backend, deadline)) = self.entries.remove(&up) else {
            return false;
        };
        self.by_deadline.remove(&(deadline, up));
        if let Some(stack) = self.stacks.get_mut(&backend) {
            stack.retain(|&idle| idle != up);
            if stack.is_empty() {
                self.stacks.remove(&backend);
            }
        }
        true
    }

    /// The idle upstream with the earliest deadline.
    pub(super) fn oldest(&self) -> Option<UpstreamId> {
        self.by_deadline.first().map(|&(_, up)| up)
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.by_deadline.first().map(|&(deadline, _)| deadline)
    }

    /// The idle upstreams due at `now`, oldest first.
    pub(super) fn due(&self, now: Instant) -> Vec<UpstreamId> {
        self.by_deadline
            .iter()
            .take_while(|(deadline, _)| *deadline <= now)
            .map(|&(_, up)| up)
            .collect()
    }

    /// Every idle upstream, oldest first.
    pub(super) fn all(&self) -> Vec<UpstreamId> {
        self.by_deadline.iter().map(|&(_, up)| up).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> Instant {
        Instant::from_nanos(secs * 1_000_000_000)
    }

    const A: BackendId = BackendId(0);
    const B: BackendId = BackendId(1);

    #[test]
    fn newest_first_per_backend_and_capped() {
        let mut pool = Pool::new();
        assert_eq!(pool.park(UpstreamId(1), A, at(1), 2), None);
        assert_eq!(pool.park(UpstreamId(2), B, at(2), 2), None);
        assert_eq!(pool.park(UpstreamId(3), A, at(3), 2), None);
        // A third for `A` pushes its oldest out; `B` is untouched.
        assert_eq!(pool.park(UpstreamId(4), A, at(4), 2), Some(UpstreamId(1)));
        assert_eq!(pool.all(), [UpstreamId(2), UpstreamId(3), UpstreamId(4)]);
        assert_eq!(pool.take(A), Some(UpstreamId(4)));
        assert_eq!(pool.take(A), Some(UpstreamId(3)));
        assert_eq!(pool.take(A), None);
        assert_eq!(pool.take(B), Some(UpstreamId(2)));
        assert_eq!(pool.next_deadline(), None);
        assert!(pool.stacks.is_empty() && pool.entries.is_empty());
    }

    #[test]
    fn deadlines_and_removal() {
        let mut pool = Pool::new();
        pool.park(UpstreamId(1), A, at(5), 8);
        pool.park(UpstreamId(2), B, at(3), 8);
        pool.park(UpstreamId(3), A, at(3), 8);
        assert_eq!(pool.next_deadline(), Some(at(3)));
        assert_eq!(pool.oldest(), Some(UpstreamId(2)));
        assert_eq!(pool.due(at(2)), []);
        assert_eq!(pool.due(at(3)), [UpstreamId(2), UpstreamId(3)]);
        assert!(pool.remove(UpstreamId(3)));
        assert!(!pool.remove(UpstreamId(3)));
        assert_eq!(pool.take(A), Some(UpstreamId(1)));
        assert_eq!(pool.all(), [UpstreamId(2)]);
    }
}
