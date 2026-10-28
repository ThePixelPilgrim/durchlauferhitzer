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
//! the sink remembers the original blocking mode and puts it back when it
//! is dropped, leaving the other flags as they are then (best effort: a
//! failure there is ignored). A process killed before that leaves the
//! description non-blocking. Only one stderr sink lives at a time
//! ([`LogSink::stderr`] refuses a second), so two cannot undo each
//! other's mode.
//!
//! On a terminal, fds 0, 1 and 2 are often one file description, so
//! stdout is non-blocking too while the sink lives, and std's `println!`
//! and `eprintln!` panic on the `EAGAIN` that can bring. Nothing may print
//! through std while a stderr sink lives: usage, `--check` output and
//! startup errors are printed before the sink is created (Task 8).

use std::io::{self, Stderr};
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

use durchlauferhitzer_core::Log;

use self::buffer::{CAPACITY, EntryBuffer};
use crate::sys::fd::{Blocking, Io, StatusFlags, set_status_flags, status_flags, write_fd};

mod buffer;
mod format;

pub use self::format::{format_driver, format_entry};

/// Whether a sink on fd 2 is live: two would undo each other's
/// `O_NONBLOCK` (one restoring blocking mode while the other still
/// writes), so [`LogSink::stderr`] refuses a second (review m1).
static STDERR_CLAIMED: AtomicBool = AtomicBool::new(false);

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
    /// The status flags before the sink set `O_NONBLOCK`; their blocking
    /// mode is put back on drop.
    original: StatusFlags,
    buffer: EntryBuffer,
    /// [`Core::backends`](durchlauferhitzer_core::Core::backends), which
    /// the core's entries name by index (FR-LOG-05); set by the driver.
    backends: Vec<SocketAddr>,
    /// Each entry is formatted here first.
    line: Vec<u8>,
    /// Released when the sink is gone (after its `Drop` put the mode
    /// back): fd 2's claim for [`LogSink::stderr`], none for
    /// [`LogSink::fd`].
    _claim: Option<Claim>,
}

/// One live sink per claim flag. A field of the sink, so it is released
/// after the sink's `Drop` restored the flags.
#[derive(Debug)]
struct Claim(&'static AtomicBool);

impl Claim {
    fn take(flag: &'static AtomicBool) -> io::Result<Claim> {
        if flag.swap(true, Ordering::AcqRel) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a log sink on this descriptor is live already",
            ));
        }
        Ok(Claim(flag))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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
        LogSink::claimed(&STDERR_CLAIMED, Out::Stderr(io::stderr()))
    }

    /// A sink on `out` that holds the claim `flag` while it lives:
    /// refused (`AlreadyExists`), before `out` is touched, while another
    /// sink holds it.
    fn claimed(flag: &'static AtomicBool, out: Out) -> io::Result<LogSink> {
        let claim = Claim::take(flag)?;
        LogSink::on(out, Some(claim))
    }

    /// A sink writing lines to `fd`, any descriptor `write(2)` works on
    /// (the tests use a pipe). The description is set to `O_NONBLOCK`
    /// until the sink is dropped.
    pub fn fd(fd: OwnedFd) -> io::Result<LogSink> {
        LogSink::on(Out::Owned(fd), None)
    }

    fn on(out: Out, claim: Option<Claim>) -> io::Result<LogSink> {
        let original = status_flags(out.as_fd())?;
        set_status_flags(out.as_fd(), original.with(Blocking::NonBlocking))?;
        Ok(LogSink {
            kind: SinkKind::Fd(FdSink {
                out,
                original,
                buffer: EntryBuffer::new(CAPACITY),
                backends: Vec::new(),
                line: Vec::new(),
                _claim: claim,
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
                // An oversized entry (an all-escaped path is four bytes
                // per byte) does not keep its capacity (review m5).
                sink.line.clear();
                sink.line.shrink_to(CAPACITY);
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
    /// Writes what it can without blocking, then puts the original
    /// blocking mode back (best effort; the other flags are left as they
    /// are now). Lines still buffered after that are lost; the
    /// shutdown flush is Task 9's.
    fn drop(&mut self) {
        self.flush();
        // Only the mode: another flag changed meanwhile stays as it is.
        if let Ok(now) = status_flags(self.out.as_fd()) {
            let _ = set_status_flags(self.out.as_fd(), now.with(self.original.mode()));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::fd::AsFd;
    use std::sync::atomic::AtomicBool;

    use std::net::{IpAddr, Ipv4Addr};

    use durchlauferhitzer_core::http::request::{HttpError, Method};
    use durchlauferhitzer_core::{ConnId, Log, RejectReason, RequestLine};

    use super::{CAPACITY, LogEntry, LogSink, Out, SinkKind};
    use crate::sys::fd::{Blocking, pipe, status_flags};

    /// Stands in for fd 2's flag, so no test touches the real stderr.
    static CLAIMED: AtomicBool = AtomicBool::new(false);

    /// Review m1: two live sinks on one description would undo each
    /// other's flags, so while one holds the claim a second is refused,
    /// before it touches its descriptor; once the first is dropped the
    /// claim is free again.
    #[test]
    fn a_second_sink_on_a_claimed_descriptor_is_refused() {
        let (_reader, writer) = pipe().unwrap();
        let first = LogSink::claimed(&CLAIMED, Out::Owned(writer)).unwrap();
        let (_reader, writer) = pipe().unwrap();
        let other = writer.try_clone().unwrap();
        let Err(err) = LogSink::claimed(&CLAIMED, Out::Owned(writer)) else {
            panic!("a second sink got the claim");
        };
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            status_flags(other.as_fd()).unwrap().mode(),
            Blocking::Blocking
        );
        drop(first);
        LogSink::claimed(&CLAIMED, Out::Owned(other)).unwrap();
    }

    /// Review m5: the scratch line does not keep the capacity of an
    /// oversized entry (an all-escaped path is four bytes per byte).
    #[test]
    fn the_scratch_line_shrinks_after_an_oversized_entry() {
        let (_reader, writer) = pipe().unwrap();
        let mut sink = LogSink::fd(writer).unwrap();
        sink.write(LogEntry::Core(Log::Rejected {
            conn: ConnId(1),
            peer: IpAddr::V4(Ipv4Addr::LOCALHOST),
            reason: RejectReason::Http {
                error: HttpError::BadHeaderValue,
                request: Some(RequestLine {
                    method: Method::Get,
                    path: " ".repeat(CAPACITY),
                }),
            },
        }));
        let SinkKind::Fd(fd_sink) = &sink.kind else {
            panic!("not an fd sink");
        };
        assert!(
            fd_sink.line.capacity() <= CAPACITY,
            "{}",
            fd_sink.line.capacity()
        );
    }
}
