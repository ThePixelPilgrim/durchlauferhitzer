//! The sim's view of the core's idle pool (FR-PROXY-02): which
//! upstreams are idle, the checks that a byte, end of stream or error on
//! one closes it at once and that a drain empties the pool (Review Focus
//! 1 and 5), and the count of idle closes by cause, for the nets'
//! coverage.

use std::collections::BTreeMap;

use durchlauferhitzer_core::{UpstreamHolder, UpstreamId};

use crate::Sim;

/// Why the core closed an idle pooled upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IdleClose {
    /// A byte arrived while it was idle.
    Byte,
    /// The backend ended its stream while it was idle.
    Eof,
    /// Its socket failed while it was idle.
    Error,
    /// `backend_idle_timeout` ran out.
    Timeout,
    /// Room was needed: `backend_idle_max` for its backend, or
    /// `max_connections` for a new connect.
    Room,
    /// A drain or the abort closed the pool.
    Drain,
    /// Not idle any more but about to be reused: its silence check found
    /// bytes, an end of stream or a reset (FR-PROXY-02).
    Busy,
}

impl Sim {
    /// While `hold` is set, the sim does not read idle pooled upstreams:
    /// what a backend sends to one waits, as when the driver has not yet
    /// handled its readiness, so only the silence check before a reuse
    /// (`CheckSilent`) can see it.
    pub fn hold_idle_reads(&mut self, hold: bool) {
        self.backends.hold_idle = hold;
    }

    /// Pump the upstreams alone until nothing moves, before any client or
    /// disk acts: what a backend sent last (a response's end and whatever
    /// follows it, its end of stream or reset) is read in one readiness
    /// event, as a driver reads until the socket would block. Then
    /// `run_until_idle` as usual.
    pub fn settle_upstreams(&mut self) {
        let mut steps: usize = 0;
        while self.pump_upstreams() {
            steps += 1;
            if steps > 1_000_000 {
                self.violation("settle_upstreams does not settle");
            }
        }
        self.run_until_idle();
    }

    /// The upstreams idle in the core's pool now.
    pub(crate) fn pooled_upstreams(&self) -> Vec<UpstreamId> {
        self.backends
            .open_ids()
            .into_iter()
            .filter(|up| self.core.upstream_holder(*up) == Some(UpstreamHolder::Pool))
            .collect()
    }

    /// Count the idle upstreams of `pooled` the last call closed, by
    /// cause: the event's own upstream by its kind, any at a `TimerFired`
    /// by the idle timeout, any other by the room a park or connect
    /// needed (`backend_idle_max`, `max_connections`).
    pub(crate) fn count_idle_closes(
        &mut self,
        pooled: &[UpstreamId],
        target: Option<UpstreamId>,
        cause: IdleClose,
    ) {
        for up in pooled {
            if self.core.upstream_holder(*up).is_some() {
                continue;
            }
            let why = match (target, cause) {
                (Some(t), _) if t == *up => cause,
                (_, IdleClose::Timeout) => IdleClose::Timeout,
                (
                    _,
                    IdleClose::Byte
                    | IdleClose::Eof
                    | IdleClose::Error
                    | IdleClose::Room
                    | IdleClose::Busy,
                ) => IdleClose::Room,
                (_, IdleClose::Drain) => IdleClose::Drain,
            };
            *self.idle_closes.entry(why).or_default() += 1;
        }
    }

    /// How many idle pooled upstreams the core closed so far, by cause
    /// (FR-PROXY-02).
    pub fn idle_closes(&self) -> &BTreeMap<IdleClose, usize> {
        &self.idle_closes
    }

    /// Review Focus 5: a drain closes the idle pool at once.
    pub(crate) fn drained_pool(&mut self, pooled: &[UpstreamId]) {
        self.backends.drained = true;
        if let Some(up) = self.pooled_upstreams().first() {
            self.violation(&format!("up={} stays idle in the pool after a drain", up.0));
        }
        self.count_idle_closes(pooled, None, IdleClose::Drain);
    }
}
