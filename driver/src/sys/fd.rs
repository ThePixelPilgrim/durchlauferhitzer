//! Shared plumbing: the outcome of a non-blocking transfer ([`Io`]) and the
//! `-1`/errno checks every wrapper uses; plus the descriptor-level calls
//! of the log sink (FR-LOG-06): `write(2)`, the file status flags
//! (`O_NONBLOCK`), `poll(2)` for writability (the bounded final flush),
//! and `pipe2` (its tests write to a pipe).

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

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

/// Like [`cvt_retry`], for calls that return a signed size (`ssize_t`,
/// or `syscall`'s `c_long`) rather than a `c_int`: `-1` is the error in
/// errno, any other value is returned as it is.
pub(crate) fn cvt_retry_ssize<T>(mut call: impl FnMut() -> T) -> io::Result<T>
where
    T: Copy + PartialEq + From<i8>,
{
    loop {
        let ret = call();
        if ret != T::from(-1) {
            return Ok(ret);
        }
        let err = io::Error::last_os_error();
        if !errno_is(&err, libc::EINTR) {
            return Err(err);
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

/// Whether [`wait_writable`] saw the descriptor writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writable {
    /// Writable, or in error or hung up (the next write reports which),
    /// or the wait was interrupted (`EINTR`): try a write.
    Ready,
    /// The timeout passed first.
    TimedOut,
}

/// Waits with `poll(2)` until `fd` is writable or `timeout` has passed
/// (rounded up to whole milliseconds, capped at `i32::MAX` ms): the log
/// sink's bounded final flush (R3). Unlike epoll, `poll` takes any
/// descriptor; a regular file or `/dev/null` is always writable.
pub fn wait_writable(fd: BorrowedFd<'_>, timeout: std::time::Duration) -> io::Result<Writable> {
    let ms = i32::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(i32::MAX);
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: `poll` is one valid, writable `pollfd`, and the count is 1.
    match cvt(unsafe { libc::poll(&mut poll, 1, ms) }) {
        Ok(0) => Ok(Writable::TimedOut),
        Ok(_) => Ok(Writable::Ready),
        Err(err) if errno_is(&err, libc::EINTR) => Ok(Writable::Ready),
        Err(err) => Err(err),
    }
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

/// Serialises [`claim_inherited`]: its check and its marking must not
/// interleave with another claim of the same number.
static CLAIMS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Takes ownership of `raw`, a descriptor this process inherited from
/// the one that started it (systemd's sockets, FR-PROC-02), and marks it
/// close-on-exec.
///
/// Sound because of how the descriptor is told apart from the process's
/// own: every descriptor Rust's std and this crate create is close-on-exec
/// from the start, so an open descriptor without the flag was inherited
/// and nothing owns it yet; once claimed it has the flag, so a second
/// claim fails. Refused: the standard descriptors (0–2), a number that is
/// not open (`EBADF`), and one with close-on-exec set (`InvalidInput`).
///
/// Crate-private (review A2): the argument holds only for a process
/// whose own descriptors all come from std and this crate. Code outside
/// the crate may open one without the flag (`libc::open`, a C library),
/// which this would then take from its owner; inside the crate only the
/// systemd sockets (`systemd.rs`) are claimed, at startup.
pub(crate) fn claim_inherited(raw: RawFd) -> io::Result<OwnedFd> {
    if raw < 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a standard descriptor cannot be claimed",
        ));
    }
    let _guard = CLAIMS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: `F_GETFD` takes no pointer argument; a number that is not
    // open is reported as `EBADF`.
    let flags = cvt(unsafe { libc::fcntl(raw, libc::F_GETFD) })?;
    if flags & libc::FD_CLOEXEC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "close-on-exec is set: the descriptor was not inherited",
        ));
    }
    // SAFETY: `F_SETFD` takes an integer, no pointer.
    cvt(unsafe { libc::fcntl(raw, libc::F_SETFD, flags | libc::FD_CLOEXEC) })?;
    // SAFETY: `raw` is open and was inherited without close-on-exec, so
    // nothing in this process owns it (see above); the flag now set makes
    // every later claim of it fail, so this is its only owner.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::os::fd::AsRawFd;

    use super::claim_inherited;

    /// `claim_inherited` refuses what this process created itself (std
    /// sets close-on-exec), the standard descriptors and a number that is
    /// not open; the positive case (systemd's fd 3) runs in the binary
    /// (`tests/systemd.rs`).
    #[test]
    fn claim_inherited_refuses_own_and_standard_descriptors() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let err = claim_inherited(file.as_raw_fd()).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        for raw in [0, 1, 2] {
            let err = claim_inherited(raw).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{raw}: {err}");
        }
        let err = claim_inherited(1 << 20).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EBADF), "{err}");
    }
}
