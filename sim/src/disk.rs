//! The disk side of the sim: the file ids the core handed out, disk
//! operations in flight (`OpenFile`, `Readahead`) completing after the disk
//! delay, `sendfile` straight from the in-memory file system, and the file
//! invariants.

use std::time::Duration;

use durchlauferhitzer_core::{
    Action, ConnId, Event, FileError, FileId, FileSegment, FileStamp, Instant, RootId,
};

use crate::fs::{Key, SimFile};
use crate::{Sim, SimConnState};

/// A disk operation in flight.
#[derive(Debug)]
pub(crate) enum DiskOp {
    Open { file: FileId },
    Read { file: FileId, offset: u64, len: u64 },
}

impl DiskOp {
    pub(crate) fn file(&self) -> FileId {
        match *self {
            DiskOp::Open { file } | DiskOp::Read { file, .. } => file,
        }
    }
}

/// Where a file id the core handed out is, as the driver sees it.
#[derive(Debug)]
pub(crate) enum Phase {
    /// `OpenFile` not yet answered.
    Opening,
    /// `FileOpened` delivered; the driver holds a descriptor. `ready` are
    /// the ranges whose `ChunkReady` was delivered. `handle` is the file as
    /// it was opened (`None` for a directory): reads and `sendfile` go
    /// through it, like through a descriptor, whatever later happens to the
    /// path.
    Open {
        size: u64,
        chunks: usize,
        ready: Vec<(u64, u64)>,
        handle: Option<SimFile>,
    },
    /// Closed by `CloseFile`, or its open failed.
    Closed,
}

#[derive(Debug)]
pub(crate) struct FileTrack {
    /// The connection whose event led to the `OpenFile`.
    pub(crate) conn: ConnId,
    pub(crate) root: String,
    pub(crate) key: Key,
    pub(crate) phase: Phase,
}

impl Sim {
    /// A regular file `path` (relative, `/`-separated) under `root`, with
    /// any missing parent directories.
    pub fn add_file(&mut self, root: &str, path: &str, bytes: &[u8]) {
        self.fs.add_file(root, path, bytes);
    }

    /// A directory `path` under `root`.
    pub fn add_dir(&mut self, root: &str, path: &str) {
        self.fs.add_dir(root, path);
    }

    /// A symlink `path` under `root`. It is never followed, so `target`
    /// only documents the test.
    pub fn add_symlink(&mut self, root: &str, path: &str, target: &str) {
        let _ = target;
        self.fs.add_symlink(root, path);
    }

    /// A FIFO, socket or device `path` under `root`: it opens as
    /// `FileKind::Other`.
    pub fn add_special(&mut self, root: &str, path: &str) {
        self.fs.add_special(root, path);
    }

    /// Opening `path` under `root`, or anything beneath it, fails with
    /// permission denied.
    pub fn deny(&mut self, root: &str, path: &str) {
        self.fs.deny(root, path);
    }

    /// Every later open and readahead completes after `d` of virtual time.
    pub fn set_disk_delay(&mut self, d: Duration) {
        self.disk_delay = d;
    }

    /// `sendfile` of `path` hits end of file at offset `at`.
    pub fn shrink_on_send(&mut self, root: &str, path: &str, at: u64) {
        self.fs.shrink_on_send(root, path, at);
    }

    /// After `after_chunks` chunks of an open `path` were read, later
    /// chunks report the stamp `new`.
    pub fn change_during_send(
        &mut self,
        root: &str,
        path: &str,
        after_chunks: usize,
        new: FileStamp,
    ) {
        self.fs.change_during_send(root, path, after_chunks, new);
    }

    /// Files the driver currently holds open (`FileOpened` delivered, no
    /// `CloseFile` yet).
    pub fn open_files(&self) -> usize {
        self.files
            .values()
            .filter(|t| matches!(t.phase, Phase::Open { .. }))
            .count()
    }

    /// Whether an `OpenFile` emitted for `conn` is not answered yet (as
    /// the driver sees it: no `FileOpened` or `FileFailed` delivered).
    pub fn opening_file(&self, conn: ConnId) -> bool {
        self.files
            .values()
            .any(|t| t.conn == conn && matches!(t.phase, Phase::Opening))
    }

    /// Deliver every disk completion that is due.
    pub(crate) fn pump_disk(&mut self) -> bool {
        let mut progress = false;
        while self.disk.front().is_some_and(|(due, _)| *due <= self.now) {
            let (_, op) = self.disk.pop_front().unwrap();
            progress = true;
            self.complete(op);
        }
        progress
    }

    pub(crate) fn complete(&mut self, op: DiskOp) {
        let file = op.file();
        let Some(track) = self.files.get_mut(&file) else {
            self.violation(&format!("disk completion for unknown file={}", file.0));
        };
        let event = match op {
            DiskOp::Open { .. } => match self.fs.open(&track.root, &track.key) {
                Ok(meta) => {
                    track.phase = Phase::Open {
                        size: meta.stamp.size,
                        chunks: 0,
                        ready: Vec::new(),
                        handle: self.fs.file(&track.root, &track.key).cloned(),
                    };
                    Event::FileOpened { file, meta }
                }
                Err(error) => {
                    track.phase = Phase::Closed;
                    Event::FileFailed { file, error }
                }
            },
            DiskOp::Read { offset, len, .. } => {
                let Phase::Open {
                    chunks,
                    ready,
                    handle,
                    ..
                } = &mut track.phase
                else {
                    panic!("readahead completion for file={} not open", file.0);
                };
                let Some(f) = handle else {
                    panic!("readahead of file={}, which is not a regular file", file.0);
                };
                let stamp = match f.change {
                    Some((after, new)) if *chunks >= after => new,
                    Some(_) | None => f.stamp,
                };
                *chunks += 1;
                ready.push((offset, offset + len));
                Event::ChunkReady { file, stamp }
            }
        };
        self.deliver(event);
    }

    /// `sendfile` of `segment` to `conn`, as far as the window and the
    /// file's end allow.
    pub(crate) fn pump_sendfile(&mut self, conn: ConnId, segment: FileSegment) -> bool {
        let window = self.window(conn) as u64;
        if window == 0 {
            return false;
        }
        if self.conn(conn).state == SimConnState::WriteShutdown {
            self.violation(&format!("conn={} sendfile after ShutdownWrite", conn.0));
        }
        let file = segment.file;
        let Some(FileTrack {
            phase: Phase::Open {
                handle: Some(f), ..
            },
            ..
        }) = self.files.get(&file)
        else {
            self.violation(&format!("sendfile of file={}, not an open file", file.0));
        };
        let eof = (f.bytes.len() as u64).min(f.shrink_at.unwrap_or(u64::MAX));
        let available = eof.saturating_sub(segment.offset).min(segment.len);
        if available == 0 {
            self.deliver(Event::FileFailed {
                file,
                error: FileError::Truncated,
            });
            return true;
        }
        let n = available.min(window);
        let start = segment.offset as usize;
        let bytes = f.bytes[start..start + n as usize].to_vec();
        self.take_window(conn, n as usize);
        self.conn_mut(conn).received.extend_from_slice(&bytes);
        self.deliver(Event::FileSent { conn, len: n });
        true
    }

    /// Queue a disk completion, ordered by due time (after any already
    /// queued for the same instant).
    pub(crate) fn queue_disk(&mut self, due: Instant, op: DiskOp) {
        let at = self.disk.partition_point(|(d, _)| *d <= due);
        self.disk.insert(at, (due, op));
    }

    /// Perform a file action: start the disk operation or close the file.
    pub(crate) fn apply_file(&mut self, action: Action) {
        let due = self.now.saturating_add(self.disk_delay);
        match action {
            Action::OpenFile { file, root, path } => {
                let Some(conn) = self.context else {
                    self.violation(&format!(
                        "OpenFile file={} outside a connection's event",
                        file.0
                    ));
                };
                let expected = self.files.keys().next_back().map_or(1, |f| f.0 + 1);
                if file.0 != expected {
                    self.violation(&format!(
                        "OpenFile file={} but the next id is {expected}",
                        file.0
                    ));
                }
                let RootId(index) = root;
                let Some(root) = self.core.static_roots().get(index as usize) else {
                    self.violation(&format!(
                        "OpenFile file={} for unknown root={index}",
                        file.0
                    ));
                };
                let Some(root) = root.to_str().map(str::to_owned) else {
                    self.violation(&format!("root {root:?} is not UTF-8"));
                };
                self.files.insert(
                    file,
                    FileTrack {
                        conn,
                        root,
                        key: path,
                        phase: Phase::Opening,
                    },
                );
                self.queue_disk(due, DiskOp::Open { file });
            }
            Action::Readahead { file, offset, len } => {
                let size = match self.files.get(&file).map(|t| &t.phase) {
                    Some(Phase::Open { size, .. }) => *size,
                    Some(Phase::Opening | Phase::Closed) | None => {
                        self.violation(&format!("Readahead for unknown or closed file={}", file.0))
                    }
                };
                if len == 0 || offset.checked_add(len).is_none_or(|end| end > size) {
                    self.violation(&format!(
                        "Readahead file={} offset={offset} len={len} beyond the size {size} \
                         seen at open",
                        file.0
                    ));
                }
                self.queue_disk(due, DiskOp::Read { file, offset, len });
            }
            Action::CloseFile { file } => {
                match self.files.get_mut(&file).map(|t| &mut t.phase) {
                    Some(phase @ Phase::Open { .. }) => *phase = Phase::Closed,
                    Some(Phase::Closed) => {
                        self.violation(&format!("second CloseFile for file={}", file.0))
                    }
                    Some(Phase::Opening) => {
                        self.violation(&format!("CloseFile before FileOpened for file={}", file.0))
                    }
                    None => self.violation(&format!("CloseFile for unknown file={}", file.0)),
                }
                // Nothing more is delivered for a closed file.
                self.disk.retain(|(_, op)| op.file() != file);
            }
            Action::SetInterest { .. }
            | Action::ShutdownWrite { .. }
            | Action::Close { .. }
            | Action::Log(_)
            | Action::Connect { .. }
            | Action::SetUpstreamInterest { .. }
            | Action::CloseUpstream { .. } => unreachable!("not a file action"),
        }
    }

    /// No file stays open once its connection is closed, and a segment is
    /// offered only for an open file of that connection, within a range
    /// whose `ChunkReady` was delivered, while the send buffer is empty.
    pub(crate) fn check_files(&self) {
        for (file, track) in &self.files {
            if matches!(track.phase, Phase::Open { .. })
                && self.conn(track.conn).state == SimConnState::Closed
            {
                self.violation(&format!(
                    "file={} left open after conn={} closed",
                    file.0, track.conn.0
                ));
            }
        }
        for (conn, c) in &self.conns {
            if c.state == SimConnState::Closed {
                continue;
            }
            let Some(segment) = self.core.send_file(*conn) else {
                continue;
            };
            if !self.core.send_buffer(*conn).is_empty() {
                self.violation(&format!(
                    "conn={} send_file returns a segment while send_buffer is non-empty",
                    conn.0
                ));
            }
            let file = segment.file;
            let ready = match self.files.get(&file) {
                Some(FileTrack {
                    conn: owner,
                    phase: Phase::Open { ready, .. },
                    ..
                }) if owner == conn => ready,
                Some(_) | None => self.violation(&format!(
                    "conn={} send_file names file={}, not an open file of it",
                    conn.0, file.0
                )),
            };
            let end = segment.offset + segment.len;
            if segment.len == 0 || !ready.iter().any(|&(a, b)| a <= segment.offset && end <= b) {
                self.violation(&format!(
                    "conn={} send_file segment {}..{end} of file={} before its ChunkReady",
                    conn.0, segment.offset, file.0
                ));
            }
        }
    }
}
