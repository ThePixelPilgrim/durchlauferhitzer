//! `--ready-fd <n>` (FR-PROC-01, FR-PROC-07 step 3): the descriptor the
//! process that started the server passed it, written `ready\n` and
//! closed once startup is complete.
//!
//! - [`ReadyFd::claim`] runs right after the arguments, before the config
//!   is read: `n` must be at least 3, not one of systemd's sockets, open
//!   without close-on-exec (inherited, [`claim_inherited`], which then
//!   sets the flag) and not a listening socket. Otherwise exit 2.
//! - The claimed descriptor stays open through startup (the budget counts
//!   it, NFR-RES-05) and is never a listener. A start that fails exits
//!   with it unwritten (the process exit closes it).
//! - [`Readiness::report`] runs after the seccomp filter (step 10a),
//!   right before the loop.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, OwnedFd, RawFd};

use super::Failure;
use crate::sys::fd::{Io, claim_inherited, write_fd};
use crate::sys::socket::{Listening, is_listening_stream};
use crate::systemd::announced_fds;

/// What readiness is reported with.
const READY: &[u8] = b"ready\n";

/// The `n` of `--ready-fd <n>`, not yet checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ReadyFd(RawFd);

impl ReadyFd {
    /// A decimal number of ASCII digits only (no sign) that fits a
    /// descriptor; anything else is `None` (the usage).
    pub(super) fn parse(arg: &OsStr) -> Option<ReadyFd> {
        let digits = arg.to_str()?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().map(ReadyFd)
    }

    /// Checks `n` and takes the descriptor (see the module docs). The
    /// `Err` is the startup error, `--ready-fd <n>: <reason>`.
    pub(super) fn claim(self) -> Result<Readiness, Failure> {
        let n = self.0;
        let refuse = |reason: &dyn std::fmt::Display| Failure(format!("--ready-fd {n}: {reason}"));
        if n < 3 {
            return Err(refuse(&"a standard descriptor (0-2) cannot be used"));
        }
        if announced_fds().contains(&n) {
            return Err(refuse(
                &"one of systemd's sockets (LISTEN_FDS) cannot be used",
            ));
        }
        let fd = claim_inherited(n).map_err(|err| match err.raw_os_error() {
            Some(libc::EBADF) => refuse(&"not open"),
            _ => refuse(&err),
        })?;
        match is_listening_stream(fd.as_fd()) {
            Ok(Listening::No) => Ok(Readiness(fd)),
            Ok(Listening::Yes) => Err(refuse(&"a listening socket cannot be used")),
            Err(err) => Err(refuse(&err)),
        }
    }
}

/// The claimed `--ready-fd` descriptor.
#[derive(Debug)]
pub(super) struct Readiness(OwnedFd);

impl Readiness {
    /// Writes `ready\n` (continuing after a short write; `EINTR` is
    /// retried) and closes the descriptor, whatever the outcome. The
    /// descriptor must have room for these 6 bytes: a full blocking pipe
    /// would block the server here, before its loop. A non-blocking
    /// descriptor that is full is `EAGAIN`; a write of 0 bytes is
    /// `WriteZero` (no errno).
    pub(super) fn report(self) -> io::Result<()> {
        let mut rest = READY;
        while !rest.is_empty() {
            match write_fd(self.0.as_fd(), rest) {
                Io::Done(n) => rest = rest.get(n..).unwrap_or_default(),
                Io::WouldBlock => return Err(io::Error::from_raw_os_error(libc::EAGAIN)),
                Io::Closed => return Err(io::ErrorKind::WriteZero.into()),
                Io::Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::io::Read as _;

    use super::{Readiness, ReadyFd};
    use crate::sys::fd::pipe;

    #[test]
    fn only_plain_decimal_numbers_parse() {
        let parse = |arg: &str| ReadyFd::parse(OsStr::new(arg));
        assert_eq!(parse("3"), Some(ReadyFd(3)));
        assert_eq!(parse("0"), Some(ReadyFd(0)));
        assert_eq!(parse("007"), Some(ReadyFd(7)));
        assert_eq!(parse("2147483647"), Some(ReadyFd(i32::MAX)));
        for bad in ["", "x", "-1", "+3", " 3", "3 ", "2147483648", "0x3"] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn report_writes_ready_and_closes() {
        let (read, write) = pipe().unwrap();
        Readiness(write).report().unwrap();
        let mut got = Vec::new();
        // EOF: the only write end is closed.
        std::fs::File::from(read).read_to_end(&mut got).unwrap();
        assert_eq!(got, b"ready\n");
    }

    #[test]
    fn report_to_a_closed_reader_is_an_error() {
        // Rust ignores SIGPIPE in test binaries, so this is EPIPE.
        let (read, write) = pipe().unwrap();
        drop(read);
        let err = Readiness(write).report().unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EPIPE), "{err}");
    }
}
