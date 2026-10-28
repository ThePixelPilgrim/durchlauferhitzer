//! Shared plumbing: the outcome of a non-blocking transfer ([`Io`]) and the
//! `-1`/errno checks every wrapper uses; plus the descriptor-level calls
//! of the log sink (FR-LOG-06): `write(2)`, the file status flags
//! (`O_NONBLOCK`), and `pipe2` (its tests write to a pipe).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// The outcome of one non-blocking read, write or `sendfile`.
#[derive(Debug)]
pub enum Io {
    /// This many bytes moved. Only 0 when the buffer (or length) was empty,
    /// in which case no system call was made.
    Done(usize),
    /// `EAGAIN`: nothing moved; wait for readiness.
    WouldBlock,
    /// A read returned 0 (the peer's end of stream), or `sendfile` returned
    /// 0 (the input file ended before the requested length).
    Closed,
    /// Any other error, with its errno.
    Err(io::Error),
}

/// Checks a return value of the `-1`-and-errno kind.
pub(crate) fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// Like [`cvt`], retrying while the call is interrupted (`EINTR`).
pub(crate) fn cvt_retry(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        match cvt(call()) {
            Err(err) if errno_is(&err, libc::EINTR) => {}
            other => return other,
        }
    }
}

/// Turns the result of `read`/`send`/`sendfile` into an [`Io`], retrying
/// on `EINTR`.
pub(crate) fn transfer(mut call: impl FnMut() -> libc::ssize_t) -> Io {
    loop {
        let ret = call();
        if ret == 0 {
            return Io::Closed;
        }
        match usize::try_from(ret) {
            Ok(n) => return Io::Done(n),
            Err(_) => {
                let err = io::Error::last_os_error();
                if errno_is(&err, libc::EAGAIN) {
                    return Io::WouldBlock;
                }
                if !errno_is(&err, libc::EINTR) {
                    return Io::Err(err);
                }
            }
        }
    }
}

/// Whether `err` carries exactly this errno.
pub(crate) fn errno_is(err: &io::Error, errno: libc::c_int) -> bool {
    err.raw_os_error() == Some(errno)
}

/// Whether a file description is in non-blocking mode (`O_NONBLOCK`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocking {
    Blocking,
    NonBlocking,
}

/// The file status flags of an open file description (`F_GETFL`). They
/// belong to the description, not the descriptor: every duplicate of it
/// (another process's stderr included) sees a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFlags(libc::c_int);

impl StatusFlags {
    pub fn mode(self) -> Blocking {
        if self.0 & libc::O_NONBLOCK == 0 {
            Blocking::Blocking
        } else {
            Blocking::NonBlocking
        }
    }

    /// The same flags with `O_NONBLOCK` set or cleared.
    pub fn with(self, mode: Blocking) -> StatusFlags {
        match mode {
            Blocking::Blocking => StatusFlags(self.0 & !libc::O_NONBLOCK),
            Blocking::NonBlocking => StatusFlags(self.0 | libc::O_NONBLOCK),
        }
    }
}

/// `fcntl(F_GETFL)`.
pub fn status_flags(fd: BorrowedFd<'_>) -> io::Result<StatusFlags> {
    // SAFETY: `F_GETFL` takes no pointer argument.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) }).map(StatusFlags)
}

/// `fcntl(F_SETFL)`. Linux changes only `O_APPEND`, `O_ASYNC`,
/// `O_DIRECT`, `O_NOATIME` and `O_NONBLOCK` this way and ignores the rest
/// of `flags`, so flags read with [`status_flags`] can be written back
/// as they are.
pub fn set_status_flags(fd: BorrowedFd<'_>, flags: StatusFlags) -> io::Result<()> {
    // SAFETY: `F_SETFL` takes an integer, no pointer.
    cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags.0) }).map(|_| ())
}

/// Writes from `buf` with `write(2)`, for descriptors that may not be
/// sockets (the log sink's stderr). A reader that went away is `EPIPE`
/// only while `SIGPIPE` is ignored or blocked (Rust binaries ignore it;
/// the driver blocks it, A9). An empty `buf` returns `Done(0)` without a
/// system call.
pub fn write_fd(fd: BorrowedFd<'_>, buf: &[u8]) -> Io {
    if buf.is_empty() {
        return Io::Done(0);
    }
    transfer(|| {
        // SAFETY: `buf` is readable for `buf.len()` bytes.
        unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) }
    })
}

/// A pipe, close-on-exec, both ends blocking: `(read end, write end)`.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1 as libc::c_int; 2];
    // SAFETY: `fds` is writable for the two descriptors `pipe2` stores.
    cvt(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    let [read, write] = fds;
    // SAFETY: `pipe2` succeeded, so `read` is a new descriptor nothing
    // else owns.
    let read = unsafe { OwnedFd::from_raw_fd(read) };
    // SAFETY: as above, for `write`.
    let write = unsafe { OwnedFd::from_raw_fd(write) };
    Ok((read, write))
}
