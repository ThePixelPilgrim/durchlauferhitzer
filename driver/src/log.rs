//! Where the loop's log entries go: the core's [`Log`]s and the driver's
//! own [`DriverLog`]s (FR-LOG-07).
//!
//! Stage 4 Task 5 has only the channel sink the tests read; the
//! `key=value` formatting and the non-blocking stderr sink are Task 7
//! (FR-LOG-05/06), which extends both types here.

use std::sync::mpsc::{self, Receiver, Sender};

use durchlauferhitzer_core::Log;

/// A log entry of the driver itself, not of the core (FR-LOG-07).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverLog {
    /// `accept` failed for a reason that is not transient: `EMFILE`,
    /// `ENFILE`, `ENOBUFS`, `ENOMEM` or an unexpected errno. After
    /// `EMFILE`/`ENFILE` the listeners pause until a connection closes.
    AcceptFailed { errno: i32 },
    /// `socket()` for a backend connect failed with `EMFILE` or `ENFILE`:
    /// the descriptor budget (NFR-RES-05) is wrong. The connect is answered
    /// `UpstreamFailed` (a 502) as the contract says.
    SocketFailed { errno: i32 },
    /// `epoll_ctl` failed for one client or upstream socket (DRV-15): that
    /// socket is reported failed (a client accepted but not yet announced
    /// is dropped); the loop goes on.
    RegisterFailed { errno: i32 },
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
