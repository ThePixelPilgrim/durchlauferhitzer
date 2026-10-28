//! The shutdown state machine (FR-PROC-05, A9, DRV-14): what each signal
//! and the shutdown deadline mean for the loop. It decides; the driver
//! closes the listeners, calls the core and leaves the loop to match.
//!
//! - Running: the first SIGTERM starts the drain, with its deadline at
//!   now plus `shutdown_timeout` (a driver-owned deadline, folded into
//!   the wait's timeout like the accept retry, DRV-16).
//! - Draining: a second SIGTERM, a SIGINT or the deadline aborts. The
//!   loop leaves once the core is drained.
//! - SIGINT while running aborts at once, without a drain.
//! - Aborted: the loop leaves at once (DRV-14: no wait for orphaned
//!   opens); nothing is asked of it any more.
//! - SIGPIPE and SIGHUP are ignored in every state (SIGHUP: stage 4; the
//!   restart signal is stage 6).

use std::time::Duration;

use durchlauferhitzer_core::Instant;

use crate::sys::signal::Signal;

/// Where the shutdown stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Running,
    /// The drain runs until the core is drained or `deadline`.
    Draining {
        deadline: Instant,
    },
    /// `abort_all` was called: the loop leaves.
    Aborted,
}

/// What the driver must do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Close the listeners, then `begin_drain`.
    Drain,
    /// `abort_all`, then leave the loop.
    Abort,
    /// Nothing.
    Ignore,
}

#[derive(Debug)]
pub(super) struct ShutdownGate {
    phase: Phase,
    /// `limits.shutdown_timeout`.
    timeout: Duration,
}

impl ShutdownGate {
    pub(super) fn new(timeout: Duration) -> ShutdownGate {
        ShutdownGate {
            phase: Phase::Running,
            timeout,
        }
    }

    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    /// The drain's deadline, while draining.
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Draining { deadline } => Some(deadline),
            Phase::Running | Phase::Aborted => None,
        }
    }

    /// `signal` arrived at `now`.
    pub(super) fn signal(&mut self, signal: Signal, now: Instant) -> Step {
        match (self.phase, signal) {
            (_, Signal::Pipe | Signal::Hup) | (Phase::Aborted, Signal::Term | Signal::Int) => {
                Step::Ignore
            }
            (Phase::Running, Signal::Term) => {
                self.phase = Phase::Draining {
                    deadline: now.saturating_add(self.timeout),
                };
                Step::Drain
            }
            (Phase::Running, Signal::Int)
            | (Phase::Draining { .. }, Signal::Term | Signal::Int) => {
                self.phase = Phase::Aborted;
                Step::Abort
            }
        }
    }

    /// The clock reached `now`: a drain whose deadline has passed aborts.
    pub(super) fn tick(&mut self, now: Instant) -> Step {
        match self.phase {
            Phase::Draining { deadline } if deadline <= now => {
                self.phase = Phase::Aborted;
                Step::Abort
            }
            Phase::Draining { .. } | Phase::Running | Phase::Aborted => Step::Ignore,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Instant {
        Instant::from_nanos(millis * 1_000_000)
    }

    fn gate() -> ShutdownGate {
        ShutdownGate::new(Duration::from_millis(500))
    }

    fn draining_until(millis: u64) -> Phase {
        Phase::Draining {
            deadline: at(millis),
        }
    }

    #[test]
    fn the_first_sigterm_drains_until_now_plus_the_timeout() {
        let mut gate = gate();
        assert_eq!(gate.phase(), Phase::Running);
        assert_eq!(gate.deadline(), None);
        assert_eq!(gate.signal(Signal::Term, at(100)), Step::Drain);
        assert_eq!(gate.phase(), draining_until(600));
        assert_eq!(gate.deadline(), Some(at(600)));
    }

    #[test]
    fn a_second_sigterm_or_a_sigint_aborts_the_drain() {
        for signal in [Signal::Term, Signal::Int] {
            let mut gate = gate();
            gate.signal(Signal::Term, at(0));
            assert_eq!(gate.signal(signal, at(10)), Step::Abort, "{signal:?}");
            assert_eq!(gate.phase(), Phase::Aborted);
            assert_eq!(gate.deadline(), None);
        }
    }

    #[test]
    fn sigint_aborts_at_once_without_a_drain() {
        let mut gate = gate();
        assert_eq!(gate.signal(Signal::Int, at(0)), Step::Abort);
        assert_eq!(gate.phase(), Phase::Aborted);
    }

    #[test]
    fn the_deadline_aborts_the_drain_at_its_instant_not_before() {
        let mut gate = gate();
        assert_eq!(gate.tick(at(10_000)), Step::Ignore);
        gate.signal(Signal::Term, at(0));
        assert_eq!(gate.tick(at(499)), Step::Ignore);
        assert_eq!(gate.phase(), draining_until(500));
        assert_eq!(gate.tick(at(500)), Step::Abort);
        assert_eq!(gate.phase(), Phase::Aborted);
        assert_eq!(gate.tick(at(501)), Step::Ignore);
    }

    #[test]
    fn sigpipe_and_sighup_are_ignored_in_every_phase() {
        let mut gate = gate();
        for signal in [Signal::Pipe, Signal::Hup] {
            assert_eq!(gate.signal(signal, at(0)), Step::Ignore);
            assert_eq!(gate.phase(), Phase::Running);
        }
        gate.signal(Signal::Term, at(0));
        for signal in [Signal::Pipe, Signal::Hup] {
            assert_eq!(gate.signal(signal, at(1)), Step::Ignore);
            assert_eq!(gate.phase(), draining_until(500));
        }
        gate.signal(Signal::Int, at(2));
        for signal in [Signal::Pipe, Signal::Hup, Signal::Term, Signal::Int] {
            assert_eq!(gate.signal(signal, at(3)), Step::Ignore);
            assert_eq!(gate.phase(), Phase::Aborted);
        }
    }
}
