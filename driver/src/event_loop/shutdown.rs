//! The shutdown state machine (FR-PROC-05, FR-PROC-07, A9, DRV-14):
//! what each signal and the drain's deadline mean for the loop. It
//! decides; the driver closes the listeners, calls the core and leaves
//! the loop to match.
//!
//! - Running: the first SIGTERM starts the shutdown drain, with its
//!   deadline at now plus `shutdown_timeout`; SIGHUP starts the restart
//!   drain, with its deadline at now plus `restart_timeout` (a
//!   driver-owned deadline, folded into the wait's timeout like the
//!   accept retry, DRV-16).
//! - Draining (either target): SIGTERM, SIGINT or the deadline aborts;
//!   SIGHUP is ignored (a second restart, or a shutdown's drain, which
//!   is final). The loop leaves once the core is drained.
//! - SIGINT while running aborts at once, without a drain.
//! - Aborted: the loop leaves at once (DRV-14: no wait for orphaned
//!   opens); nothing is asked of it any more.
//! - SIGPIPE is ignored in every state.

use std::time::Duration;

use durchlauferhitzer_core::{DrainTarget, Instant};

use crate::sys::signal::Signal;

/// Where the shutdown stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Running,
    /// The drain for `target` runs until the core is drained or
    /// `deadline`.
    Draining {
        deadline: Instant,
        target: DrainTarget,
    },
    /// `abort_all` was called: the loop leaves.
    Aborted,
}

/// What the driver must do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Close the listeners, then `begin_drain` with this target.
    Drain(DrainTarget),
    /// `abort_all`, then leave the loop.
    Abort,
    /// Nothing.
    Ignore,
}

#[derive(Debug)]
pub(super) struct ShutdownGate {
    phase: Phase,
    /// `limits.shutdown_timeout`: bounds the SIGTERM drain.
    shutdown_timeout: Duration,
    /// `limits.restart_timeout`: bounds the SIGHUP drain.
    restart_timeout: Duration,
}

impl ShutdownGate {
    pub(super) fn new(shutdown_timeout: Duration, restart_timeout: Duration) -> ShutdownGate {
        ShutdownGate {
            phase: Phase::Running,
            shutdown_timeout,
            restart_timeout,
        }
    }

    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    /// The drain's deadline, while draining.
    pub(super) fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Draining { deadline, .. } => Some(deadline),
            Phase::Running | Phase::Aborted => None,
        }
    }

    /// `signal` arrived at `now`.
    pub(super) fn signal(&mut self, signal: Signal, now: Instant) -> Step {
        match (self.phase, signal) {
            (_, Signal::Pipe)
            | (Phase::Draining { .. }, Signal::Hup)
            | (Phase::Aborted, Signal::Term | Signal::Int | Signal::Hup) => Step::Ignore,
            (Phase::Running, Signal::Term) => self.drain(DrainTarget::Shutdown, now),
            (Phase::Running, Signal::Hup) => self.drain(DrainTarget::Restart, now),
            (Phase::Running, Signal::Int)
            | (Phase::Draining { .. }, Signal::Term | Signal::Int) => {
                self.phase = Phase::Aborted;
                Step::Abort
            }
        }
    }

    fn drain(&mut self, target: DrainTarget, now: Instant) -> Step {
        let timeout = match target {
            DrainTarget::Shutdown => self.shutdown_timeout,
            DrainTarget::Restart => self.restart_timeout,
        };
        self.phase = Phase::Draining {
            deadline: now.saturating_add(timeout),
            target,
        };
        Step::Drain(target)
    }

    /// The clock reached `now`: a drain whose deadline has passed aborts.
    pub(super) fn tick(&mut self, now: Instant) -> Step {
        match self.phase {
            Phase::Draining { deadline, .. } if deadline <= now => {
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

    /// `shutdown_timeout` 500 ms, `restart_timeout` 2 s.
    fn gate() -> ShutdownGate {
        ShutdownGate::new(Duration::from_millis(500), Duration::from_millis(2000))
    }

    fn draining_until(millis: u64) -> Phase {
        Phase::Draining {
            deadline: at(millis),
            target: DrainTarget::Shutdown,
        }
    }

    fn restarting_until(millis: u64) -> Phase {
        Phase::Draining {
            deadline: at(millis),
            target: DrainTarget::Restart,
        }
    }

    #[test]
    fn the_first_sigterm_drains_until_now_plus_the_timeout() {
        let mut gate = gate();
        assert_eq!(gate.phase(), Phase::Running);
        assert_eq!(gate.deadline(), None);
        assert_eq!(
            gate.signal(Signal::Term, at(100)),
            Step::Drain(DrainTarget::Shutdown)
        );
        assert_eq!(gate.phase(), draining_until(600));
        assert_eq!(gate.deadline(), Some(at(600)));
    }

    #[test]
    fn sighup_while_running_drains_for_a_restart_until_now_plus_restart_timeout() {
        let mut gate = gate();
        assert_eq!(
            gate.signal(Signal::Hup, at(100)),
            Step::Drain(DrainTarget::Restart)
        );
        assert_eq!(gate.phase(), restarting_until(2100));
        assert_eq!(gate.deadline(), Some(at(2100)));
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
    fn sigterm_or_sigint_aborts_the_restart_drain() {
        for signal in [Signal::Term, Signal::Int] {
            let mut gate = gate();
            gate.signal(Signal::Hup, at(0));
            assert_eq!(gate.signal(signal, at(10)), Step::Abort, "{signal:?}");
            assert_eq!(gate.phase(), Phase::Aborted);
            assert_eq!(gate.deadline(), None);
        }
    }

    #[test]
    fn a_second_sighup_during_the_restart_drain_is_ignored() {
        let mut gate = gate();
        gate.signal(Signal::Hup, at(0));
        assert_eq!(gate.signal(Signal::Hup, at(10)), Step::Ignore);
        assert_eq!(gate.phase(), restarting_until(2000));
    }

    #[test]
    fn sighup_during_the_sigterm_drain_is_ignored() {
        let mut gate = gate();
        gate.signal(Signal::Term, at(0));
        assert_eq!(gate.signal(Signal::Hup, at(10)), Step::Ignore);
        assert_eq!(gate.phase(), draining_until(500));
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
    fn the_restart_deadline_aborts_at_restart_timeout_not_before() {
        let mut gate = gate();
        gate.signal(Signal::Hup, at(0));
        assert_eq!(gate.tick(at(500)), Step::Ignore);
        assert_eq!(gate.tick(at(1999)), Step::Ignore);
        assert_eq!(gate.phase(), restarting_until(2000));
        assert_eq!(gate.tick(at(2000)), Step::Abort);
        assert_eq!(gate.phase(), Phase::Aborted);
        assert_eq!(gate.tick(at(2001)), Step::Ignore);
    }

    #[test]
    fn sigpipe_is_ignored_in_every_phase_and_everything_after_an_abort() {
        let mut gate = gate();
        assert_eq!(gate.signal(Signal::Pipe, at(0)), Step::Ignore);
        assert_eq!(gate.phase(), Phase::Running);
        gate.signal(Signal::Term, at(0));
        assert_eq!(gate.signal(Signal::Pipe, at(1)), Step::Ignore);
        assert_eq!(gate.phase(), draining_until(500));
        let mut restart = self::gate();
        restart.signal(Signal::Hup, at(0));
        assert_eq!(restart.signal(Signal::Pipe, at(1)), Step::Ignore);
        assert_eq!(restart.phase(), restarting_until(2000));
        gate.signal(Signal::Int, at(2));
        for signal in [Signal::Pipe, Signal::Hup, Signal::Term, Signal::Int] {
            assert_eq!(gate.signal(signal, at(3)), Step::Ignore);
            assert_eq!(gate.phase(), Phase::Aborted);
        }
    }
}
