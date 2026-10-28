//! Whether the listeners take new connections (DRV-16): an `accept` that
//! fails for a reason that is not transient pauses every listener until
//! the driver closes a descriptor or [`RETRY`] has passed, so the loop
//! never spins on a listener it cannot serve and never stops for good.

use std::io;
use std::time::Duration;

use durchlauferhitzer_core::Instant;

/// How long the listeners stay paused when no descriptor is closed.
pub(super) const RETRY: Duration = Duration::from_secs(1);

/// What an `accept` error means for the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AcceptError {
    /// The connection went away before it was taken (`ECONNABORTED`,
    /// `EPROTO`) or a firewall refused it (`EPERM`): skip it and go on.
    Transient,
    /// Anything else (`EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM`, an
    /// unexpected errno; 0 without one): log it and pause the listeners.
    Pause(i32),
}

pub(super) fn classify(err: &io::Error) -> AcceptError {
    match err.raw_os_error() {
        Some(libc::ECONNABORTED | libc::EPROTO | libc::EPERM) => AcceptError::Transient,
        Some(errno) => AcceptError::Pause(errno),
        None => AcceptError::Pause(0),
    }
}

/// Whether the listeners are in the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Accepting {
    Open,
    /// Out of the wait until a descriptor is closed or `retry_at`.
    Paused {
        retry_at: Instant,
    },
}

/// Whether an accept failure is logged: at most one line per [`RETRY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Logging {
    Log,
    Quiet,
}

/// Whether a call put the listeners back into the wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Resume {
    Resumed,
    Unchanged,
}

/// The pause state machine. It decides; the driver changes the listeners'
/// registrations to match.
#[derive(Debug)]
pub(super) struct AcceptGate {
    state: Accepting,
    /// When the last accept failure was logged.
    logged: Option<Instant>,
}

impl AcceptGate {
    pub(super) fn new() -> AcceptGate {
        AcceptGate {
            state: Accepting::Open,
            logged: None,
        }
    }

    pub(super) fn state(&self) -> Accepting {
        self.state
    }

    /// The instant the pause ends by itself, while paused.
    pub(super) fn retry_at(&self) -> Option<Instant> {
        match self.state {
            Accepting::Open => None,
            Accepting::Paused { retry_at } => Some(retry_at),
        }
    }

    /// `accept` failed at `now` for a reason that is not transient: pause
    /// until `now + RETRY`. Logged unless a failure was logged less than
    /// `RETRY` ago.
    pub(super) fn failed(&mut self, now: Instant) -> Logging {
        self.state = Accepting::Paused {
            retry_at: now.saturating_add(RETRY),
        };
        let quiet = self
            .logged
            .is_some_and(|logged| logged.saturating_add(RETRY) > now);
        if quiet {
            return Logging::Quiet;
        }
        self.logged = Some(now);
        Logging::Log
    }

    /// The driver closed a descriptor: a paused gate opens at once.
    pub(super) fn descriptor_closed(&mut self) -> Resume {
        self.open()
    }

    /// Opens a paused gate whose retry instant has passed.
    pub(super) fn tick(&mut self, now: Instant) -> Resume {
        match self.state {
            Accepting::Paused { retry_at } if retry_at <= now => self.open(),
            Accepting::Paused { .. } | Accepting::Open => Resume::Unchanged,
        }
    }

    fn open(&mut self) -> Resume {
        match self.state {
            Accepting::Open => Resume::Unchanged,
            Accepting::Paused { .. } => {
                self.state = Accepting::Open;
                Resume::Resumed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(millis: u64) -> Instant {
        Instant::from_nanos(millis * 1_000_000)
    }

    fn paused_until(millis: u64) -> Accepting {
        Accepting::Paused {
            retry_at: at(millis),
        }
    }

    #[test]
    fn only_aborted_protocol_and_firewall_errors_are_transient() {
        for errno in [libc::ECONNABORTED, libc::EPROTO, libc::EPERM] {
            let err = io::Error::from_raw_os_error(errno);
            assert_eq!(classify(&err), AcceptError::Transient, "{errno}");
        }
        for errno in [
            libc::EMFILE,
            libc::ENFILE,
            libc::ENOBUFS,
            libc::ENOMEM,
            libc::EINVAL,
        ] {
            let err = io::Error::from_raw_os_error(errno);
            assert_eq!(classify(&err), AcceptError::Pause(errno), "{errno}");
        }
        let err = io::Error::other("no errno");
        assert_eq!(classify(&err), AcceptError::Pause(0));
    }

    #[test]
    fn a_failure_pauses_for_one_second_and_is_logged() {
        let mut gate = AcceptGate::new();
        assert_eq!(gate.state(), Accepting::Open);
        assert_eq!(gate.retry_at(), None);
        assert_eq!(gate.failed(at(5)), Logging::Log);
        assert_eq!(gate.state(), paused_until(1005));
        assert_eq!(gate.retry_at(), Some(at(1005)));
    }

    #[test]
    fn the_timer_resumes_at_the_retry_instant_not_before() {
        let mut gate = AcceptGate::new();
        gate.failed(at(0));
        assert_eq!(gate.tick(at(999)), Resume::Unchanged);
        assert_eq!(gate.state(), paused_until(1000));
        assert_eq!(gate.tick(at(1000)), Resume::Resumed);
        assert_eq!(gate.state(), Accepting::Open);
        assert_eq!(gate.tick(at(1001)), Resume::Unchanged);
    }

    /// A failure on the timer's retry logs once more and pauses again: one
    /// retry and one line per second.
    #[test]
    fn a_failure_on_the_timed_retry_logs_and_pauses_again() {
        let mut gate = AcceptGate::new();
        gate.failed(at(0));
        gate.tick(at(1000));
        assert_eq!(gate.failed(at(1000)), Logging::Log);
        assert_eq!(gate.state(), paused_until(2000));
    }

    /// A closed descriptor resumes at once; a failure right after it pauses
    /// again but is not logged within a second of the last line.
    #[test]
    fn a_closed_descriptor_resumes_at_once_and_logs_stay_one_per_second() {
        let mut gate = AcceptGate::new();
        assert_eq!(gate.descriptor_closed(), Resume::Unchanged);
        gate.failed(at(0));
        assert_eq!(gate.descriptor_closed(), Resume::Resumed);
        assert_eq!(gate.state(), Accepting::Open);
        assert_eq!(gate.failed(at(100)), Logging::Quiet);
        assert_eq!(gate.state(), paused_until(1100));
        gate.descriptor_closed();
        assert_eq!(gate.failed(at(1000)), Logging::Log);
        assert_eq!(gate.state(), paused_until(2000));
    }
}
