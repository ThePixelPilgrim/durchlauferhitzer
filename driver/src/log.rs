//! Where the loop's log entries go: the core's [`Log`]s and the driver's
//! own [`DriverLog`]s (FR-LOG-07), and how they are written: one
//! `key=value` line each ([`format_entry`], [`format_driver`],
//! FR-LOG-05).

use std::sync::mpsc::{self, Receiver, Sender};

use durchlauferhitzer_core::Log;

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

    /// Writes one entry.
    pub fn write(&mut self, entry: LogEntry) {
        match &self.kind {
            SinkKind::Channel(sender) => {
                // A test that stopped listening does not want it.
                let _ = sender.send(entry);
            }
        }
    }
}
