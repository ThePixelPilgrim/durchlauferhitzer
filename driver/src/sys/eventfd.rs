//! An `eventfd` counter: worker threads wake the event loop with it.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use super::fd::{cvt, cvt_retry_ssize, errno_is};

/// A non-blocking, close-on-exec `eventfd`. It is readable (for epoll)
/// while its counter is non-zero. `notify` may be called from any thread.
#[derive(Debug)]
pub struct EventFd {
    fd: OwnedFd,
}

impl EventFd {
    pub fn new() -> io::Result<EventFd> {
        // SAFETY: `eventfd` takes no pointers.
        let fd = cvt(unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) })?;
        // SAFETY: `fd` was just created and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(EventFd { fd })
    }

    /// A second descriptor for the same counter (`F_DUPFD_CLOEXEC`, through
    /// std's `OwnedFd::try_clone`): a `notify` through either wakes an
    /// epoll set that holds the other. The disk pool's workers get one
    /// (R1); it counts in the descriptor budget (NFR-RES-05).
    pub fn try_clone(&self) -> io::Result<EventFd> {
        Ok(EventFd {
            fd: self.fd.try_clone()?,
        })
    }

    /// Adds 1 to the counter. A counter at its maximum (`EAGAIN`) is
    /// already readable, so that is not an error.
    pub fn notify(&self) -> io::Result<()> {
        let one: u64 = 1;
        let ret = cvt_retry_ssize(|| {
            // SAFETY: `one` is readable for the 8 bytes written.
            unsafe {
                libc::write(
                    self.fd.as_raw_fd(),
                    (&raw const one).cast::<libc::c_void>(),
                    size_of::<u64>(),
                )
            }
        });
        match ret {
            Ok(_) => Ok(()),
            Err(err) if errno_is(&err, libc::EAGAIN) => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Reads and resets the counter: the number of `notify` calls since the
    /// last drain, 0 if there were none.
    pub fn drain(&self) -> io::Result<u64> {
        let mut count: u64 = 0;
        let ret = cvt_retry_ssize(|| {
            // SAFETY: `count` is writable for the 8 bytes read.
            unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&raw mut count).cast::<libc::c_void>(),
                    size_of::<u64>(),
                )
            }
        });
        match ret {
            Ok(_) => Ok(count),
            Err(err) if errno_is(&err, libc::EAGAIN) => Ok(0),
            Err(err) => Err(err),
        }
    }
}

impl AsFd for EventFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
