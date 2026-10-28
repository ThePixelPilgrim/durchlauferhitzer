//! Where the loop's log entries go: the core's [`Log`]s and the driver's
//! own [`DriverLog`]s (FR-LOG-07), and how they are written: one
//! `key=value` line each ([`format_entry`], [`format_driver`],
//! FR-LOG-05).
//!
//! The production sink writes to stderr without ever blocking the loop
//! (FR-LOG-06): the file description is set to `O_NONBLOCK`, lines that
//! do not go out at once wait in a 64 KiB buffer of whole entries, and
//! the loop writes them on `EPOLLOUT` of the descriptor, for which it
//! registers only while bytes wait ([`LogSink::buffered`]).
//!
//! `O_NONBLOCK` belongs to the file description, which stderr shares with
//! whoever started the process (an interactive shell's terminal, say):
//! the sink remembers the original flags and puts them back when it is
//! dropped (best effort: a failure there is ignored). A process killed
//! before that leaves the description non-blocking.

use std::io::{self, Stderr};
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::mpsc::{self, Receiver, Sender};

use durchlauferhitzer_core::Log;

use self::buffer::{CAPACITY, EntryBuffer};
use crate::sys::fd::{Blocking, Io, StatusFlags, set_status_flags, status_flags, write_fd};

mod buffer;
mod format;

pub use self::format::{format_driver, format_entry};

/// A log entry of the driver itself, not of the core (FR-LOG-07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverLog {
    /// `accept` failed for a reason that is not transient: `EMFILE`,
    /// `ENFILE`, `ENOBUFS`, `ENOMEM` or an unexpected errno (0 without
    /// one). The listeners pause until the driver closes a descriptor or
    /// 1 s has passed; logged at most once a second (DRV-16).
    AcceptFailed { errno: i32 },
    /// `socket()` for a backend connect failed with `EMFILE` or `ENFILE`:
    /// the descriptor budget (NFR-RES-05) is wrong. The connect is answered
    /// `UpstreamFailed` (a 502) as the contract says.
    SocketFailed { errno: i32 },
    /// Opening a static file failed with `EMFILE` or `ENFILE`: the
    /// descriptor budget (NFR-RES-05) is wrong, not the file missing. The
    /// core still gets `FileFailed { Other }`, so the client gets a 404
    /// (the contract's errno table).
    OpenFailed { errno: i32 },
    /// `epoll_ctl` failed for one client or upstream socket (DRV-15): that
    /// socket is reported failed (a client accepted but not yet announced
    /// is dropped); the loop goes on.
    RegisterFailed { errno: i32 },
    /// Entries dropped because the sink's buffer was full (FR-LOG-06).
    Dropped { count: u64 },
}

/// One entry handed to a [`LogSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEntry {
    Core(Log),
    Driver(DriverLog),
}

/// Whether a sink holds bytes it could not write yet (the loop then waits
/// for the descriptor to become writable, and only then).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Buffered {
    Nothing,
    Bytes,
}

/// Where log entries are written. Writing never blocks the loop.
#[derive(Debug)]
pub struct LogSink {
    kind: SinkKind,
}

#[derive(Debug)]
enum SinkKind {
    /// Entries go to a channel (tests). A receiver that went away drops
    /// them.
    Channel(Sender<LogEntry>),
    /// Entries are written as lines to a descriptor (FR-LOG-05/06).
    Fd(FdSink),
}

/// The descriptor a sink writes to.
#[derive(Debug)]
enum Out {
    Owned(OwnedFd),
    /// fd 2, which the sink must not close.
    Stderr(Stderr),
}

impl AsFd for Out {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Out::Owned(fd) => fd.as_fd(),
            Out::Stderr(stderr) => stderr.as_fd(),
        }
    }
}

#[derive(Debug)]
struct FdSink {
    out: Out,
    /// The status flags before the sink set `O_NONBLOCK`, put back on drop.
    original: StatusFlags,
    buffer: EntryBuffer,
    /// [`Core::backends`](durchlauferhitzer_core::Core::backends), which
    /// the core's entries name by index (FR-LOG-05); set by the driver.
    backends: Vec<SocketAddr>,
    /// Each entry is formatted here first.
    line: Vec<u8>,
}

impl LogSink {
    /// A sink that sends every entry to the returned receiver.
    pub fn channel() -> (LogSink, Receiver<LogEntry>) {
        let (sender, receiver) = mpsc::channel();
        let sink = LogSink {
            kind: SinkKind::Channel(sender),
        };
        (sink, receiver)
    }

    /// The production sink: lines to stderr (fd 2, which stays open when
    /// the sink is dropped). Fails if fd 2 is not open.
    pub fn stderr() -> io::Result<LogSink> {
        LogSink::on(Out::Stderr(io::stderr()))
    }

    /// A sink writing lines to `fd`, any descriptor `write(2)` works on
    /// (the tests use a pipe). The description is set to `O_NONBLOCK`
    /// until the sink is dropped.
    pub fn fd(fd: OwnedFd) -> io::Result<LogSink> {
        LogSink::on(Out::Owned(fd))
    }

    fn on(out: Out) -> io::Result<LogSink> {
        let original = status_flags(out.as_fd())?;
        set_status_flags(out.as_fd(), original.with(Blocking::NonBlocking))?;
        Ok(LogSink {
            kind: SinkKind::Fd(FdSink {
                out,
                original,
                buffer: EntryBuffer::new(CAPACITY),
                backends: Vec::new(),
                line: Vec::new(),
            }),
        })
    }

    /// Writes one entry: formatted, queued behind what waits (a full
    /// buffer drops it and counts it), and written as far as the
    /// descriptor takes it now.
    pub fn write(&mut self, entry: LogEntry) {
        match &mut self.kind {
            SinkKind::Channel(sender) => {
                // A test that stopped listening does not want it.
                let _ = sender.send(entry);
            }
            SinkKind::Fd(sink) => {
                sink.line.clear();
                match &entry {
                    LogEntry::Core(log) => format_entry(log, &sink.backends, &mut sink.line),
                    LogEntry::Driver(log) => format_driver(log, &mut sink.line),
                }
                sink.buffer.push(&sink.line);
                sink.flush();
            }
        }
    }

    /// Writes what waits, as far as the descriptor takes it now (the loop
    /// calls this when the descriptor is writable).
    pub fn flush(&mut self) {
        match &mut self.kind {
            SinkKind::Channel(_) => {}
            SinkKind::Fd(sink) => sink.flush(),
        }
    }

    /// The backends the core's entries name by index: the driver passes
    /// [`Core::backends`](durchlauferhitzer_core::Core::backends), so a
    /// line carries the address (FR-LOG-05). Until then an id is written
    /// as its index.
    pub(crate) fn set_backends(&mut self, backends: &[SocketAddr]) {
        match &mut self.kind {
            SinkKind::Fd(sink) => backends.clone_into(&mut sink.backends),
            SinkKind::Channel(_) => {}
        }
    }

    /// Whether bytes wait to be written.
    pub fn buffered(&self) -> Buffered {
        match &self.kind {
            SinkKind::Fd(sink) if !sink.buffer.is_empty() => Buffered::Bytes,
            SinkKind::Fd(_) | SinkKind::Channel(_) => Buffered::Nothing,
        }
    }

    /// The descriptor the loop waits on while bytes are buffered; `None`
    /// for the channel sink.
    pub(crate) fn out_fd(&self) -> Option<BorrowedFd<'_>> {
        match &self.kind {
            SinkKind::Fd(sink) => Some(sink.out.as_fd()),
            SinkKind::Channel(_) => None,
        }
    }
}

impl FdSink {
    /// Writes from the front until the buffer is empty or the descriptor
    /// would block. A hard error (`EPIPE`: the reader went away, `EIO`,
    /// ...) loses what is buffered, counted as dropped, so no bytes wait
    /// for a writability that would only report the error again.
    fn flush(&mut self) {
        loop {
            match write_fd(self.out.as_fd(), self.buffer.front()) {
                // Nothing was left to write.
                Io::Done(0) => return,
                Io::Done(n) => {
                    self.buffer.consume(n);
                    self.buffer.catch_up();
                }
                Io::WouldBlock => return,
                // `write` never reports an end of stream; treated as an
                // error all the same.
                Io::Closed | Io::Err(_) => {
                    self.buffer.discard();
                    return;
                }
            }
        }
    }
}

impl Drop for FdSink {
    /// Writes what it can without blocking, then puts the original flags
    /// back (best effort). Lines still buffered after that are lost; the
    /// shutdown flush is Task 9's.
    fn drop(&mut self) {
        self.flush();
        let _ = set_status_flags(self.out.as_fd(), self.original);
    }
}
