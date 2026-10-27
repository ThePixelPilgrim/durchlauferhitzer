//! The sim's clock: `advance` fires the core's deadlines and completes
//! disk operations, on time or as a loop that wakes late (`TimerOrder`,
//! NFR-TEST-07).

use std::time::Duration;

use durchlauferhitzer_core::{Event, Instant};

use crate::Sim;

/// How [`Sim::advance`] orders an overdue `TimerFired` against the I/O
/// and disk completions due with it (NFR-TEST-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerOrder {
    /// Every deadline fires at its own instant, before any I/O still
    /// queued; a disk completion due at the same instant goes first.
    OnTime,
    /// The loop wakes late, as a real one does when it oversleeps: at the
    /// end of the advance it delivers every queued I/O and every disk
    /// completion due by then first, then the overdue `TimerFired`.
    Late,
}

impl Sim {
    /// Move the clock forward by `by`, firing every deadline and completing
    /// every disk operation on the way. In [`TimerOrder::OnTime`] (the
    /// default) that happens in time order (disk first on a tie); in
    /// [`TimerOrder::Late`] the loop wakes once, at the end, as described
    /// there.
    pub fn advance(&mut self, by: Duration) {
        let target = self.now.saturating_add(by);
        match self.timer_order {
            TimerOrder::OnTime => self.advance_on_time(target),
            TimerOrder::Late => self.advance_late(target),
        }
        self.now = target;
    }

    /// How the sim orders an overdue `TimerFired` against the I/O and disk
    /// completions that are due with it (NFR-TEST-07).
    pub fn set_timer_order(&mut self, order: TimerOrder) {
        self.line(&format!("set_timer_order {order:?}"));
        self.timer_order = order;
    }

    /// The earlier of the core's next deadline and the next disk
    /// completion: `None` once nothing is left for `advance` to do.
    pub fn next_due(&self) -> Option<Instant> {
        let timer = self.core.next_deadline();
        let disk = self.disk.front().map(|(due, _)| *due);
        match (timer, disk) {
            (Some(t), Some(d)) => Some(t.min(d)),
            (Some(t), None) => Some(t),
            (None, Some(d)) => Some(d),
            (None, None) => None,
        }
    }

    fn advance_on_time(&mut self, target: Instant) {
        while let Some(next) = self.next_due().filter(|next| *next <= target) {
            self.now = self.now.max(next);
            let disk = self.disk.front().map(|(due, _)| *due);
            if disk.is_some_and(|d| d <= self.now) {
                self.run_until_idle();
                continue;
            }
            self.fire_timer();
        }
    }

    /// The loop slept through everything due up to `target` and wakes at
    /// `target`: every queued I/O and every disk completion due by then is
    /// delivered first, then the overdue `TimerFired`, both at `target`.
    fn advance_late(&mut self, target: Instant) {
        while self.next_due().is_some_and(|next| next <= target) {
            self.now = target;
            self.line("late wake");
            self.run_until_idle();
            if self.core.next_deadline().is_some_and(|t| t <= self.now) {
                self.fire_timer();
            }
        }
    }

    /// Deliver `TimerFired` at `now`, check that no deadline is left at or
    /// before `now`, and pump.
    fn fire_timer(&mut self) {
        self.deliver(Event::TimerFired);
        if let Some(next) = self.core.next_deadline() {
            if next <= self.now {
                self.violation(&format!(
                    "after TimerFired at t={}ns next_deadline is t={}ns, not later",
                    self.now.as_nanos(),
                    next.as_nanos()
                ));
            }
        }
        self.run_until_idle();
    }
}
