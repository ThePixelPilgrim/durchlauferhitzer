//! The vocabulary of the driver contract (see the [`crate::core`] module
//! documentation): connection ids, the events a driver delivers, the
//! actions it performs and the interest it waits for.

use std::net::IpAddr;

#[cfg(doc)]
use super::Core;
use crate::file::{FileError, FileId, FileMeta, FileStamp, RootId};
use crate::log::Log;

/// Opaque connection id, allocated by the driver and never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(pub u64);

/// Transport-level failure reported by the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnError {
    /// The peer reset the connection (`ECONNRESET`, `EPIPE`).
    Reset,
    /// Any other socket error.
    Other,
}

/// Something that happened in the outside world. Each is delivered with the
/// current time via [`Core::handle`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A new client connection was accepted. Always the first event for
    /// `conn`.
    Accepted { conn: ConnId, peer: IpAddr },
    /// The driver wrote `len` bytes into the slice returned by the last
    /// [`Core::recv_buffer`] call for `conn`, starting at its first byte.
    /// `len` is never 0 (end of stream is [`Event::ReadClosed`]). It must be
    /// the very next `handle` call after that read: no other event may come
    /// in between, or the core may have moved or reused that space.
    Received { conn: ConnId, len: usize },
    /// The peer closed its sending side (a read returned end of stream).
    ReadClosed { conn: ConnId },
    /// The socket accepted the first `len` bytes of [`Core::send_buffer`].
    Sent { conn: ConnId, len: usize },
    /// The socket failed; the driver will not do further I/O on it except
    /// what the core's resulting actions ask for (`Close`).
    Failed { conn: ConnId, error: ConnError },
    /// The instant last returned by [`Core::next_deadline`] has passed.
    /// After handling it, every deadline at or before `now` has been
    /// handled: `next_deadline` returns `None` or an instant after `now`.
    TimerFired,
    /// The `OpenFile` for `file` succeeded; `meta` describes the open
    /// descriptor.
    FileOpened { file: FileId, meta: FileMeta },
    /// An `OpenFile`, `Readahead` or `sendfile` for `file` failed.
    FileFailed { file: FileId, error: FileError },
    /// The last `Readahead` for `file` completed; `stamp` is the file's
    /// stamp when the chunk was read.
    ChunkReady { file: FileId, stamp: FileStamp },
    /// The socket accepted the first `len` bytes of the segment last
    /// returned by [`Core::send_file`] for `conn`.
    FileSent { conn: ConnId, len: u64 },
}

/// Which readiness the driver should wait for on a connection. Replaces the
/// previous interest entirely; a new connection starts at `None` until the
/// core says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interest {
    /// Wait for nothing (e.g. receive buffer full: backpressure).
    None,
    /// Read into [`Core::recv_buffer`] when readable.
    Read,
    /// Write from [`Core::send_buffer`], then from [`Core::send_file`],
    /// when writable.
    Write,
}

/// Something the driver must do, in the order emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Change what the driver waits for on `conn`.
    SetInterest { conn: ConnId, interest: Interest },
    /// Shut down the sending side of `conn` (half-close); reading may go on.
    ShutdownWrite { conn: ConnId },
    /// Close `conn`. The last action the core emits for it.
    Close { conn: ConnId },
    /// Open `path` (relative segments) beneath static root `root`, without
    /// following symlinks and without leaving the root, as `file`.
    OpenFile {
        file: FileId,
        root: RootId,
        path: Vec<Vec<u8>>,
    },
    /// Read `len` bytes of `file` from `offset` into the page cache, then
    /// report `ChunkReady`. Never beyond the size reported at open.
    Readahead { file: FileId, offset: u64, len: u64 },
    /// Close `file`; drop any completion still pending for it.
    CloseFile { file: FileId },
    /// Emit a log entry.
    Log(Log),
}
