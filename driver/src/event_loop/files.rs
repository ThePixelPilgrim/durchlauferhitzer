//! The file contract on the loop's side (DRV-07…09): `OpenFile` and
//! `Readahead` go to the disk pool, its results come back as `FileOpened`,
//! `FileFailed` and `ChunkReady`, `CloseFile` drops the loop's descriptor
//! (a worker running the file's readahead keeps it open until it drops
//! the job), and `sendfile` of the core's segment runs on the loop.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;

use durchlauferhitzer_core::{
    ConnError, ConnId, Event, FileError, FileId, FileMeta, FileStamp, RootId,
};

use super::{Driver, FileFailure, Freed};
use crate::log::{DriverLog, LogEntry};
use crate::pool::{Cancel, Job, JobResult};
use crate::sys::fd::Io;
use crate::sys::file::{Stat, file_error, sendfile};

/// Whether a readahead of an open file is with the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    Idle,
    InFlight,
}

/// Where a file id the core handed out stands in the driver.
#[derive(Debug)]
enum FileState {
    /// The `OpenFile` is with the pool.
    Opening,
    /// `CloseFile` came before the open's answer (the core never does
    /// this): the answer is dropped, its descriptor closed.
    Abandoned,
    /// `FileOpened` was delivered; `sendfile` reads from `fd`.
    Open { fd: Arc<OwnedFd>, reading: Reading },
    /// `CloseFile` came while a worker ran the file's readahead: a
    /// tombstone, holding no descriptor (I-1). The worker's job holds the
    /// last reference and closes the descriptor when the worker drops it,
    /// before it sends the result; that result is then dropped (DRV-09).
    /// A readahead still queued is cancelled instead (OA1), so at most
    /// `disk_threads` closed files hold a descriptor, each in a running job.
    Closing,
}

/// What to do with a pool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Answer {
    /// Deliver its event.
    Deliver,
    /// Drop it: the file was closed meanwhile (or never ours).
    Drop(Freed),
}

/// What a `Readahead` found.
#[derive(Debug)]
pub(super) enum StartRead {
    /// The file was open and idle: the readahead is in flight now, and the
    /// job holds this reference to the descriptor.
    Started(Arc<OwnedFd>),
    /// A readahead is already in flight (the core never asks for two):
    /// the second is answered `FileFailed { Other }`.
    Busy,
    /// The file is not open (opening, closed or unknown): nothing to read
    /// and, after `CloseFile`, nothing may be delivered for it.
    NotOpen,
}

/// The driver's file table: every file id from its `OpenFile` until its
/// `CloseFile` (or failed open), plus closed files whose readahead is
/// still in flight.
#[derive(Debug)]
pub(super) struct Files {
    map: BTreeMap<FileId, FileState>,
}

impl Files {
    pub(super) fn new() -> Files {
        Files {
            map: BTreeMap::new(),
        }
    }

    /// An `OpenFile` went to the pool.
    pub(super) fn opening(&mut self, file: FileId) {
        self.map.insert(file, FileState::Opening);
    }

    /// The open succeeded with `fd`.
    pub(super) fn opened(&mut self, file: FileId, fd: OwnedFd) -> Answer {
        match self.map.remove(&file) {
            Some(FileState::Opening) => {
                let fd = Arc::new(fd);
                self.map.insert(
                    file,
                    FileState::Open {
                        fd,
                        reading: Reading::Idle,
                    },
                );
                Answer::Deliver
            }
            Some(state @ (FileState::Open { .. } | FileState::Closing)) => {
                // Not an answer to anything in flight: keep the state.
                self.map.insert(file, state);
                Answer::Drop(Freed::Descriptor)
            }
            Some(FileState::Abandoned) | None => Answer::Drop(Freed::Descriptor),
        }
    }

    /// The open failed: nothing to close.
    pub(super) fn open_failed(&mut self, file: FileId) -> Answer {
        match self.map.remove(&file) {
            Some(FileState::Opening) => Answer::Deliver,
            Some(state @ (FileState::Open { .. } | FileState::Closing)) => {
                self.map.insert(file, state);
                Answer::Drop(Freed::Nothing)
            }
            Some(FileState::Abandoned) | None => Answer::Drop(Freed::Nothing),
        }
    }

    /// A `Readahead`: for an open, idle file it is now in flight.
    pub(super) fn start_read(&mut self, file: FileId) -> StartRead {
        match self.map.get_mut(&file) {
            Some(FileState::Open { fd, reading }) => match reading {
                Reading::Idle => {
                    *reading = Reading::InFlight;
                    StartRead::Started(Arc::clone(fd))
                }
                Reading::InFlight => StartRead::Busy,
            },
            Some(FileState::Opening | FileState::Abandoned | FileState::Closing) | None => {
                StartRead::NotOpen
            }
        }
    }

    /// A readahead's result (success or failure) arrived.
    pub(super) fn read_done(&mut self, file: FileId) -> Answer {
        match self.map.get_mut(&file) {
            Some(FileState::Open { reading, .. }) => match reading {
                Reading::InFlight => {
                    *reading = Reading::Idle;
                    Answer::Deliver
                }
                Reading::Idle => Answer::Drop(Freed::Nothing),
            },
            Some(FileState::Closing) => {
                // The worker dropped the job, and with it the last
                // reference, before sending: the descriptor is closed by
                // now, and the loop learns of it here (paused listeners
                // resume, DRV-16).
                self.map.remove(&file);
                Answer::Drop(Freed::Descriptor)
            }
            Some(FileState::Opening | FileState::Abandoned) | None => Answer::Drop(Freed::Nothing),
        }
    }

    /// `CloseFile`: the loop drops its descriptor. A readahead still
    /// queued is cancelled first (`cancel`, the pool's
    /// `Pool::cancel_readahead`; OA1), so the descriptor closes at once;
    /// one a worker runs keeps it open until the worker drops the job, and
    /// the file stays `Closing` (holding nothing) until that result
    /// arrives: at most `disk_threads` of them (NFR-RES-05).
    pub(super) fn close(&mut self, file: FileId, cancel: impl FnOnce(FileId) -> Cancel) -> Freed {
        match self.map.remove(&file) {
            Some(FileState::Open { fd, reading }) => match reading {
                Reading::Idle => Freed::Descriptor,
                Reading::InFlight => match cancel(file) {
                    // The job's reference went with it: `fd` is the last.
                    Cancel::Cancelled => Freed::Descriptor,
                    Cancel::NotQueued => {
                        // The running job holds the last reference now.
                        drop(fd);
                        self.map.insert(file, FileState::Closing);
                        Freed::Nothing
                    }
                },
            },
            Some(FileState::Opening) => {
                self.map.insert(file, FileState::Abandoned);
                Freed::Nothing
            }
            Some(state @ (FileState::Abandoned | FileState::Closing)) => {
                self.map.insert(file, state);
                Freed::Nothing
            }
            None => Freed::Nothing,
        }
    }

    /// The descriptor `sendfile` reads from: only while the file is open.
    pub(super) fn fd(&self, file: FileId) -> Option<&Arc<OwnedFd>> {
        match self.map.get(&file) {
            Some(FileState::Open { fd, .. }) => Some(fd),
            Some(FileState::Opening | FileState::Abandoned | FileState::Closing) | None => None,
        }
    }
}

/// The event a `sendfile` of `conn`'s segment of `file` reports (DRV-09):
/// the bytes the socket took; nothing for `EAGAIN` (wait for
/// writability); `Truncated` for a 0 return (end of file inside the
/// segment); `EPIPE`/`ECONNRESET` as the socket's `Failed { Reset }`; any
/// other error as `FileFailed { Other }`.
pub(super) fn sendfile_event(conn: ConnId, file: FileId, io: Io) -> Option<Event> {
    match io {
        // Only for an empty segment, which the core never hands out.
        Io::Done(0) | Io::WouldBlock => None,
        Io::Done(len) => Some(Event::FileSent {
            conn,
            len: u64::try_from(len).unwrap_or(u64::MAX),
        }),
        Io::Closed => Some(Event::FileFailed {
            file,
            error: FileError::Truncated,
        }),
        Io::Err(err) => match err.raw_os_error() {
            Some(libc::EPIPE | libc::ECONNRESET) => Some(Event::Failed {
                conn,
                error: ConnError::Reset,
            }),
            Some(_) | None => Some(Event::FileFailed {
                file,
                error: FileError::Other,
            }),
        },
    }
}

/// The metadata the core gets for an open descriptor: the stamp is
/// `st_size` plus `st_mtime` in nanoseconds (DRV-09).
fn meta(stat: &Stat) -> FileMeta {
    FileMeta {
        kind: stat.kind,
        stamp: stamp(stat),
    }
}

fn stamp(stat: &Stat) -> FileStamp {
    FileStamp {
        size: stat.size,
        modified_ns: stat.mtime_ns,
    }
}

impl Driver {
    /// `OpenFile`: the open goes to the pool (DRV-08). A root the driver
    /// has not opened, or a driver without a pool (both refused by
    /// `Driver::new`), is answered `FileFailed { Other }` after the
    /// actions of the current call.
    pub(super) fn open_file(&mut self, file: FileId, root: RootId, path: Vec<Vec<u8>>) {
        let root_fd = usize::try_from(root.0).ok().and_then(|i| self.roots.get(i));
        match (root_fd, &self.pool) {
            (Some(root_fd), Some(pool)) => {
                self.files.opening(file);
                pool.submit(Job::Open {
                    file,
                    root_fd: Arc::clone(root_fd),
                    segments: path,
                });
            }
            (None, _) | (_, None) => self.file_failures.push_back(FileFailure::Open(file)),
        }
    }

    /// `Readahead`: to the pool, for an open file with none in flight. A
    /// second one while one is in flight (never, from the core) is answered
    /// `FileFailed { Other }` after the current call; one for a file that
    /// is not open is ignored (after `CloseFile` nothing may be delivered).
    pub(super) fn readahead(&mut self, file: FileId, offset: u64, len: u64) {
        // An open file means the pool opened it; without one, nothing is
        // marked in flight, so the file's state stays consistent.
        let Some(pool) = &self.pool else {
            self.file_failures.push_back(FileFailure::Read(file));
            return;
        };
        match self.files.start_read(file) {
            StartRead::Started(fd) => pool.submit(Job::Readahead {
                file,
                fd,
                offset,
                len,
            }),
            StartRead::Busy => self.file_failures.push_back(FileFailure::Read(file)),
            StartRead::NotOpen => {}
        }
    }

    /// `CloseFile` (DRV-09): drops the loop's descriptor, and marks the
    /// file closing while a worker runs its readahead.
    pub(super) fn close_file(&mut self, file: FileId) -> io::Result<()> {
        let pool = self.pool.as_ref();
        let freed = self.files.close(file, |file| match pool {
            Some(pool) => pool.cancel_readahead(file),
            None => Cancel::NotQueued,
        });
        self.freed(freed)
    }

    /// The wake eventfd fired: every result the pool has is turned into
    /// its event (or dropped, for a file closed meanwhile), in the order
    /// they arrived (DRV-08: any order is allowed).
    pub(super) fn pool_results(&mut self) -> io::Result<()> {
        loop {
            let Some(result) = self.pool.as_ref().and_then(|pool| pool.results().next()) else {
                return Ok(());
            };
            self.pool_result(result)?;
        }
    }

    pub(super) fn pool_result(&mut self, result: JobResult) -> io::Result<()> {
        let (answer, event) = match result {
            JobResult::Opened { file, fd, stat } => (
                self.files.opened(file, fd),
                Event::FileOpened {
                    file,
                    meta: meta(&stat),
                },
            ),
            JobResult::OpenFailed { file, error } => {
                let answer = self.files.open_failed(file);
                if answer == Answer::Deliver {
                    self.log_open_error(&error);
                }
                let error = file_error(&error);
                (answer, Event::FileFailed { file, error })
            }
            JobResult::ChunkRead { file, stat } => (
                self.files.read_done(file),
                Event::ChunkReady {
                    file,
                    stamp: stamp(&stat),
                },
            ),
            JobResult::ReadFailed { file, error } => {
                let error = file_error(&error);
                (
                    self.files.read_done(file),
                    Event::FileFailed { file, error },
                )
            }
        };
        match answer {
            Answer::Deliver => self.deliver(event),
            Answer::Drop(freed) => self.freed(freed),
        }
    }

    /// `EMFILE`/`ENFILE` from an open mean the descriptor budget is wrong
    /// (NFR-RES-05), not a missing file: logged by the driver; the core
    /// still gets `FileFailed { Other }` (a 404).
    fn log_open_error(&mut self, error: &io::Error) {
        if let Some(errno @ (libc::EMFILE | libc::ENFILE)) = error.raw_os_error() {
            self.log
                .write(LogEntry::Driver(DriverLog::OpenFailed { errno }));
        }
    }

    /// One `sendfile` of `conn`'s current segment, if the core has one;
    /// its event, to be delivered as the very next call (DRV-04). A
    /// segment of a file the driver does not hold open cannot come from
    /// the core; it fails the transfer rather than leave the connection
    /// waiting for a writability it would never use.
    pub(super) fn sendfile_segment(&self, conn: ConnId) -> Option<Event> {
        let segment = self.core.send_file(conn)?;
        let client = self.clients.get(&conn)?;
        let Some(fd) = self.files.fd(segment.file) else {
            return Some(Event::FileFailed {
                file: segment.file,
                error: FileError::Other,
            });
        };
        let io = sendfile(client.fd.as_fd(), fd.as_fd(), segment.offset, segment.len);
        sendfile_event(conn, segment.file, io)
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::sync::{Arc, Weak};

    use durchlauferhitzer_core::{ConnError, ConnId, Event, FileError, FileId};

    use super::{Answer, Files, Freed, StartRead, sendfile_event};
    use crate::pool::Cancel;
    use crate::sys::fd::Io;

    /// A pool that has no readahead of the file queued.
    fn running(_: FileId) -> Cancel {
        Cancel::NotQueued
    }

    fn fd() -> OwnedFd {
        OwnedFd::from(std::fs::File::open("/dev/null").unwrap())
    }

    const F: FileId = FileId(1);

    /// An open file, and a weak handle that tells whether its descriptor
    /// is still open.
    fn open_file(files: &mut Files) -> Weak<OwnedFd> {
        files.opening(F);
        assert_eq!(files.opened(F, fd()), Answer::Deliver);
        Arc::downgrade(files.fd(F).unwrap())
    }

    #[test]
    fn a_file_without_a_readahead_closes_at_once() {
        let mut files = Files::new();
        let weak = open_file(&mut files);
        assert_eq!(files.close(F, running), Freed::Descriptor);
        assert!(weak.upgrade().is_none());
        assert_eq!(files.close(F, running), Freed::Nothing);
    }

    /// DRV-09: `CloseFile` while a worker runs the readahead leaves the
    /// descriptor open only as long as the worker's job holds it (I-1,
    /// round 2): the loop keeps no reference, so the descriptor closes when
    /// the worker drops the job, before its result arrives; that result is
    /// then dropped (no event) and frees the descriptor's slot.
    #[test]
    fn close_during_a_readahead_closes_when_the_job_is_dropped() {
        let mut files = Files::new();
        let weak = open_file(&mut files);
        let StartRead::Started(job_fd) = files.start_read(F) else {
            panic!("no readahead started");
        };
        assert_eq!(files.close(F, running), Freed::Nothing);
        assert!(weak.upgrade().is_some(), "closed while the readahead runs");
        assert!(files.fd(F).is_none(), "no sendfile after CloseFile");
        // The worker drops the job before it sends the result.
        drop(job_fd);
        assert!(
            weak.upgrade().is_none(),
            "the loop still holds the closed file's descriptor"
        );
        assert_eq!(files.read_done(F), Answer::Drop(Freed::Descriptor));
        assert_eq!(files.read_done(F), Answer::Drop(Freed::Nothing));
    }

    /// OA1: `CloseFile` while the readahead is still queued cancels it, so
    /// the descriptor closes at once (and paused listeners resume); a late
    /// result for the file (none comes) would be dropped.
    #[test]
    fn close_with_a_queued_readahead_cancels_it_and_closes_at_once() {
        let mut files = Files::new();
        let weak = open_file(&mut files);
        let StartRead::Started(job_fd) = files.start_read(F) else {
            panic!("no readahead started");
        };
        let mut asked = None;
        let freed = files.close(F, |file| {
            asked = Some(file);
            // The pool drops the job, and its reference, when it cancels.
            drop(job_fd);
            Cancel::Cancelled
        });
        assert_eq!(asked, Some(F));
        assert_eq!(freed, Freed::Descriptor);
        assert!(weak.upgrade().is_none(), "the descriptor is still open");
        assert!(files.fd(F).is_none());
        assert_eq!(files.read_done(F), Answer::Drop(Freed::Nothing));
    }

    /// OA1: an idle file is closed without asking the pool.
    #[test]
    fn close_of_an_idle_file_does_not_ask_the_pool() {
        let mut files = Files::new();
        let _weak = open_file(&mut files);
        let freed = files.close(F, |_| panic!("asked the pool"));
        assert_eq!(freed, Freed::Descriptor);
    }

    #[test]
    fn a_readahead_answer_reopens_the_file_for_the_next() {
        let mut files = Files::new();
        let _weak = open_file(&mut files);
        assert!(matches!(files.start_read(F), StartRead::Started(_)));
        // One readahead at a time (the core never asks for two).
        assert!(matches!(files.start_read(F), StartRead::Busy));
        assert_eq!(files.read_done(F), Answer::Deliver);
        assert!(matches!(files.start_read(F), StartRead::Started(_)));
        assert!(matches!(files.start_read(FileId(9)), StartRead::NotOpen));
        assert!(
            files.fd(F).is_some(),
            "sendfile goes on while reading ahead"
        );
    }

    #[test]
    fn open_answers_are_delivered_once_and_strays_are_dropped() {
        let mut files = Files::new();
        files.opening(F);
        assert_eq!(files.open_failed(F), Answer::Deliver);
        assert_eq!(files.open_failed(F), Answer::Drop(Freed::Nothing));
        // An open the driver never asked for: its descriptor is closed.
        assert_eq!(
            files.opened(FileId(9), fd()),
            Answer::Drop(Freed::Descriptor)
        );
        // A readahead answer for a file that is not reading.
        assert_eq!(files.read_done(FileId(9)), Answer::Drop(Freed::Nothing));
    }

    /// The core never closes a file whose open is unanswered; if it did,
    /// the late answer is dropped (and its descriptor closed) like a
    /// readahead's after `CloseFile`.
    #[test]
    fn close_before_the_open_answer_drops_the_answer() {
        let mut files = Files::new();
        files.opening(F);
        assert_eq!(files.close(F, running), Freed::Nothing);
        assert_eq!(files.opened(F, fd()), Answer::Drop(Freed::Descriptor));
        files.opening(FileId(2));
        assert_eq!(files.close(FileId(2), running), Freed::Nothing);
        assert_eq!(files.open_failed(FileId(2)), Answer::Drop(Freed::Nothing));
    }

    /// DRV-09 / the contract's `sendfile` rules.
    #[test]
    fn sendfile_outcomes_map_to_the_contract_events() {
        let conn = ConnId(4);
        let os = |errno| Io::Err(std::io::Error::from_raw_os_error(errno));
        assert_eq!(
            sendfile_event(conn, F, Io::Done(7)),
            Some(Event::FileSent { conn, len: 7 })
        );
        assert_eq!(sendfile_event(conn, F, Io::WouldBlock), None);
        assert_eq!(sendfile_event(conn, F, Io::Done(0)), None);
        assert_eq!(
            sendfile_event(conn, F, Io::Closed),
            Some(Event::FileFailed {
                file: F,
                error: FileError::Truncated
            })
        );
        for errno in [libc::EPIPE, libc::ECONNRESET] {
            assert_eq!(
                sendfile_event(conn, F, os(errno)),
                Some(Event::Failed {
                    conn,
                    error: ConnError::Reset
                })
            );
        }
        for errno in [libc::EIO, libc::EINVAL, libc::ENOMEM] {
            assert_eq!(
                sendfile_event(conn, F, os(errno)),
                Some(Event::FileFailed {
                    file: F,
                    error: FileError::Other
                })
            );
        }
    }
}
