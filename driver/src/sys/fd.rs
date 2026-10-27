//! Shared plumbing: the outcome of a non-blocking transfer ([`Io`]) and the
//! `-1`/errno checks every wrapper uses.

use std::io;

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
