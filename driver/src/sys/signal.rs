//! Signals as readable events: the handled signals are blocked and read
//! from a `signalfd` in the epoll set (A9): SIGTERM, SIGINT, SIGPIPE and
//! SIGHUP.

use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use super::fd::{cvt, cvt_retry, errno_is};

/// The signals the driver handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGTERM`: drain, or abort if already draining.
    Term,
    /// `SIGINT`: abort.
    Int,
    /// `SIGPIPE`: blocked so it cannot kill the process (`sendfile` has no
    /// `MSG_NOSIGNAL`); read and ignored.
    Pipe,
    /// `SIGHUP`: ignored in stage 4 (A9; the restart signal is stage 6),
    /// blocked and read so its default action cannot end the server.
    Hup,
}

impl Signal {
    /// Every signal the driver blocks and reads from its signalfd.
    pub const HANDLED: [Signal; 4] = [Signal::Term, Signal::Int, Signal::Pipe, Signal::Hup];

    fn number(self) -> libc::c_int {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Int => libc::SIGINT,
            Signal::Pipe => libc::SIGPIPE,
            Signal::Hup => libc::SIGHUP,
        }
    }

    fn from_number(signo: u32) -> Option<Signal> {
        let signo = libc::c_int::try_from(signo).ok()?;
        Signal::HANDLED.into_iter().find(|s| s.number() == signo)
    }
}

/// A non-blocking, close-on-exec `signalfd` for a set of signals.
#[derive(Debug)]
pub struct SignalFd {
    fd: OwnedFd,
}

impl SignalFd {
    /// Blocks `signals` in the calling thread (`pthread_sigmask`), then
    /// opens a `signalfd` for them. Call it on the main thread before any
    /// other thread is started: new threads inherit the mask, so the
    /// signals are then never delivered the default way to any thread. If
    /// `signalfd` fails, the signals stay blocked with nothing reading
    /// them; startup exits on that error anyway.
    pub fn new(signals: &[Signal]) -> io::Result<SignalFd> {
        // SAFETY: an all-zero `sigset_t` is a valid value; `sigemptyset`
        // initialises it below anyway.
        let mut mask: libc::sigset_t = unsafe { zeroed() };
        // SAFETY: `mask` is a writable `sigset_t`.
        cvt(unsafe { libc::sigemptyset(&mut mask) })?;
        for signal in signals {
            // SAFETY: `mask` is an initialised `sigset_t`, and the number
            // is a valid signal.
            cvt(unsafe { libc::sigaddset(&mut mask, signal.number()) })?;
        }
        // SAFETY: `mask` is an initialised `sigset_t`; the old mask is not
        // wanted (null).
        let rc = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        let flags = libc::SFD_NONBLOCK | libc::SFD_CLOEXEC;
        // SAFETY: `mask` is an initialised `sigset_t` the kernel only reads.
        let fd = cvt(unsafe { libc::signalfd(-1, &mask, flags) })?;
        // SAFETY: `fd` was just created and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(SignalFd { fd })
    }

    /// Reads one pending signal; `None` when none is pending (`EAGAIN`).
    pub fn read(&self) -> io::Result<Option<Signal>> {
        // SAFETY: an all-zero `signalfd_siginfo` is a valid value.
        let mut info: libc::signalfd_siginfo = unsafe { zeroed() };
        let len = size_of::<libc::signalfd_siginfo>();
        let ret = cvt_retry(|| {
            // SAFETY: `info` is writable for `len` bytes.
            let n = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&raw mut info).cast::<libc::c_void>(),
                    len,
                )
            };
            // `len` (128) or -1.
            libc::c_int::try_from(n).unwrap_or(-1)
        });
        match ret {
            Ok(_) => Signal::from_number(info.ssi_signo)
                .map(Some)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected signal {}", info.ssi_signo),
                    )
                }),
            Err(err) if errno_is(&err, libc::EAGAIN) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl AsFd for SignalFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::{Signal, SignalFd};

    /// A thread-directed signal (controller ruling R3): a process-directed
    /// `kill(getpid(), SIGTERM)` could land on another libtest thread that
    /// has not blocked SIGTERM and end the whole test binary.
    fn raise_here(signo: libc::c_int) {
        // SAFETY: `pthread_self` has no preconditions.
        let me = unsafe { libc::pthread_self() };
        // SAFETY: `pthread_kill` targets this live thread, which has blocked
        // `signo` (SignalFd::new ran on it), so the signal stays pending
        // instead of being delivered.
        let rc = unsafe { libc::pthread_kill(me, signo) };
        assert_eq!(rc, 0);
    }

    #[test]
    fn signalfd_receives_thread_directed_signals() {
        let fd = SignalFd::new(&Signal::HANDLED).unwrap();
        assert!(matches!(fd.read(), Ok(None)));
        raise_here(libc::SIGTERM);
        assert!(matches!(fd.read(), Ok(Some(Signal::Term))));
        assert!(matches!(fd.read(), Ok(None)));
        raise_here(libc::SIGPIPE);
        assert!(matches!(fd.read(), Ok(Some(Signal::Pipe))));
        raise_here(libc::SIGINT);
        assert!(matches!(fd.read(), Ok(Some(Signal::Int))));
        raise_here(libc::SIGHUP);
        assert!(matches!(fd.read(), Ok(Some(Signal::Hup))));
        assert!(matches!(fd.read(), Ok(None)));
    }
}
