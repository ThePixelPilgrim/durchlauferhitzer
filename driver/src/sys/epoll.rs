//! A level-triggered `epoll` instance.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::time::Duration;

use super::fd::{cvt, errno_is};

/// What to wait for on a descriptor, mirroring the core's `Interest`.
/// `EPOLLERR` and `EPOLLHUP` are always reported by the kernel, also under
/// `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    None,
    Read,
    Write,
    ReadWrite,
}

impl Readiness {
    /// Whether it includes reading.
    pub fn has_read(self) -> bool {
        match self {
            Readiness::Read | Readiness::ReadWrite => true,
            Readiness::None | Readiness::Write => false,
        }
    }

    /// Whether it includes writing.
    pub fn has_write(self) -> bool {
        match self {
            Readiness::Write | Readiness::ReadWrite => true,
            Readiness::None | Readiness::Read => false,
        }
    }

    /// Both together: reading if either reads, writing if either writes.
    pub fn combine(self, other: Readiness) -> Readiness {
        match (
            self.has_read() || other.has_read(),
            self.has_write() || other.has_write(),
        ) {
            (false, false) => Readiness::None,
            (true, false) => Readiness::Read,
            (false, true) => Readiness::Write,
            (true, true) => Readiness::ReadWrite,
        }
    }

    fn bits(self) -> u32 {
        let (read, write) = (libc::EPOLLIN as u32, libc::EPOLLOUT as u32);
        match self {
            Readiness::None => 0,
            Readiness::Read => read,
            Readiness::Write => write,
            Readiness::ReadWrite => read | write,
        }
    }
}

/// The error and hangup conditions an event carries besides its
/// [`Readiness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    None,
    /// `EPOLLHUP`: both directions are shut down.
    Hangup,
    /// `EPOLLERR`: a socket error is pending (read it with `take_error`,
    /// or let the next read or write report it).
    Error,
    /// Both `EPOLLERR` and `EPOLLHUP`.
    ErrorHangup,
}

/// One ready descriptor, as filled in by [`Epoll::wait`].
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct EpollEvent(libc::epoll_event);

impl EpollEvent {
    /// A blank event, for initialising the buffer passed to [`Epoll::wait`].
    pub const EMPTY: EpollEvent = EpollEvent(libc::epoll_event { events: 0, u64: 0 });

    /// The token the descriptor was registered with.
    pub fn token(&self) -> u64 {
        self.0.u64
    }

    /// Whether it is readable (`EPOLLIN`), writable (`EPOLLOUT`) or both.
    pub fn readiness(&self) -> Readiness {
        let events = self.0.events;
        let read = events & libc::EPOLLIN as u32 != 0;
        let write = events & libc::EPOLLOUT as u32 != 0;
        match (read, write) {
            (false, false) => Readiness::None,
            (true, false) => Readiness::Read,
            (false, true) => Readiness::Write,
            (true, true) => Readiness::ReadWrite,
        }
    }

    /// Whether `EPOLLERR` and/or `EPOLLHUP` are set.
    pub fn trouble(&self) -> Trouble {
        let events = self.0.events;
        let error = events & libc::EPOLLERR as u32 != 0;
        let hangup = events & libc::EPOLLHUP as u32 != 0;
        match (error, hangup) {
            (false, false) => Trouble::None,
            (false, true) => Trouble::Hangup,
            (true, false) => Trouble::Error,
            (true, true) => Trouble::ErrorHangup,
        }
    }
}

impl std::fmt::Debug for EpollEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EpollEvent")
            .field("token", &self.token())
            .field("readiness", &self.readiness())
            .field("trouble", &self.trouble())
            .finish()
    }
}

/// An `epoll` instance (`EPOLL_CLOEXEC`). Registrations are level-triggered.
#[derive(Debug)]
pub struct Epoll {
    fd: OwnedFd,
}

impl Epoll {
    pub fn new() -> io::Result<Epoll> {
        // SAFETY: `epoll_create1` takes no pointers.
        let fd = cvt(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
        // SAFETY: `fd` is a descriptor the call just created and nothing
        // else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Epoll { fd })
    }

    /// Registers `fd` with `token`.
    pub fn add(&self, fd: BorrowedFd<'_>, token: u64, readiness: Readiness) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_ADD, fd, token, readiness)
    }

    /// Replaces the token and readiness of a registered `fd`.
    pub fn modify(&self, fd: BorrowedFd<'_>, token: u64, readiness: Readiness) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_MOD, fd, token, readiness)
    }

    /// Removes `fd` from the set. (Closing a descriptor removes it too, once
    /// no duplicate of it is left open.)
    pub fn delete(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        self.ctl(libc::EPOLL_CTL_DEL, fd, 0, Readiness::None)
    }

    fn ctl(
        &self,
        op: libc::c_int,
        fd: BorrowedFd<'_>,
        token: u64,
        readiness: Readiness,
    ) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: readiness.bits(),
            u64: token,
        };
        // SAFETY: both descriptors are open (borrowed for the call), and
        // `event` is a valid `epoll_event` the kernel only reads.
        cvt(unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), op, fd.as_raw_fd(), &mut event) })?;
        Ok(())
    }

    /// Waits for events, filling `events` from the front; returns how many.
    /// `None` waits indefinitely. A timeout is rounded up to whole
    /// milliseconds, so the call never returns before it has passed (unless
    /// an event arrives), and is capped at `i32::MAX` ms. An interrupted wait
    /// (`EINTR`) returns 0 events, so the caller recomputes its timeout.
    pub fn wait(&self, events: &mut [EpollEvent], timeout: Option<Duration>) -> io::Result<usize> {
        let timeout_ms = match timeout {
            None => -1,
            Some(d) => {
                let ms = d.as_nanos().div_ceil(1_000_000);
                i32::try_from(ms).unwrap_or(i32::MAX)
            }
        };
        let max = i32::try_from(events.len()).unwrap_or(i32::MAX);
        // SAFETY: `EpollEvent` is `repr(transparent)` over `epoll_event`, so
        // the buffer holds `events.len()` valid, writable `epoll_event`s, and
        // `max` never exceeds that length.
        let ret = unsafe {
            libc::epoll_wait(
                self.fd.as_raw_fd(),
                events.as_mut_ptr().cast::<libc::epoll_event>(),
                max,
                timeout_ms,
            )
        };
        match cvt(ret) {
            Ok(n) => Ok(usize::try_from(n).unwrap_or(0)),
            Err(err) if errno_is(&err, libc::EINTR) => Ok(0),
            Err(err) => Err(err),
        }
    }
}

impl AsFd for Epoll {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
