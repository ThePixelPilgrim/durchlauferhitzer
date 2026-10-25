//! Simulation driver for the durchlauferhitzer core (test-only).
//!
//! `Sim` stands in for the network and the clock. It follows the same driver
//! contract as the production epoll driver (see `durchlauferhitzer_core::Core`):
//! it reads client bytes directly into `Core::recv_buffer`, writes directly
//! out of `Core::send_buffer`, performs actions in order and never reuses a
//! `ConnId`. Every event and action is recorded in [`Sim::trace`], and any
//! violation of the contract by the core panics with the trace attached.
//!
//! Client-side calls (`connect`, `reset`) deliver their event at once;
//! `send` and `send_eof` only queue data. [`Sim::run_until_idle`] pumps the
//! queues through the core until nothing moves.
//!
//! Files live in an in-memory file system (see `fs`), one tree per static
//! root, named by the root's path as written in the config. Disk operations
//! (`OpenFile`, `Readahead`) complete after [`Sim::set_disk_delay`] of
//! virtual time: with no delay inside `run_until_idle`, otherwise inside
//! [`Sim::advance`], merged with the core's deadlines. `sendfile` copies
//! straight from the file system into the client's received bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod fs;

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{
    Action, ConnError, ConnId, Core, Event, FileError, FileId, FileSegment, FileStamp, Instant,
    Interest, Log, RootId,
};

use crate::fs::{Fs, Key, SimFile};

/// Default bound on pump steps in one `run_until_idle` before the sim
/// declares a livelock.
const STEP_LIMIT: usize = 1_000_000;

/// How many bytes the simulated client accepts per `Sent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Unlimited,
    Bytes(usize),
    Stalled,
    /// Accept at most this many more bytes in total, then stall (the budget
    /// shrinks with every `Sent`).
    Budget(usize),
}

/// Connection state as the simulated network sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimConnState {
    Open,
    WriteShutdown,
    Closed,
}

/// Whether the client still talks to the server on this connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientSide {
    Live,
    /// The client reset the connection; no more I/O events are delivered.
    Reset,
}

/// Progress of the client's end-of-stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Eof {
    NotSent,
    Queued,
    Delivered,
}

#[derive(Debug)]
struct SimConn {
    to_server: VecDeque<u8>,
    eof: Eof,
    client: ClientSide,
    received: Vec<u8>,
    read_chunk: usize,
    window: Window,
    interest: Interest,
    state: SimConnState,
    /// Send buffer capacity the core reported right after `Accepted`; it
    /// must not change while the connection is open. (The receive capacity
    /// may change with the connection's state, e.g. on entering a lingering
    /// close, so it is checked against the core's current report instead.)
    /// 0 until recorded.
    send_capacity: usize,
}

/// A disk operation in flight.
#[derive(Debug)]
enum DiskOp {
    Open { file: FileId },
    Read { file: FileId, offset: u64, len: u64 },
}

impl DiskOp {
    fn file(&self) -> FileId {
        match *self {
            DiskOp::Open { file } | DiskOp::Read { file, .. } => file,
        }
    }
}

/// Where a file id the core handed out is, as the driver sees it.
#[derive(Debug)]
enum Phase {
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
struct FileTrack {
    /// The connection whose event led to the `OpenFile`.
    conn: ConnId,
    root: String,
    key: Key,
    phase: Phase,
}

/// The simulated network, clock and client population.
#[derive(Debug)]
pub struct Sim {
    core: Core,
    now: Instant,
    next_id: u64,
    conns: BTreeMap<ConnId, SimConn>,
    logs: Vec<Log>,
    trace: String,
    actions: Vec<Action>,
    /// Set while bytes written into a connection's `recv_buffer` have not yet
    /// been reported with `Received`.
    pending_read: Option<ConnId>,
    step_limit: usize,
    fs: Fs,
    disk_delay: Duration,
    /// Disk completions in order of their due time.
    disk: VecDeque<(Instant, DiskOp)>,
    files: BTreeMap<FileId, FileTrack>,
    /// The connection the event being handled belongs to, if any.
    context: Option<ConnId>,
}

impl Sim {
    /// A simulation of a server loaded from `config`. Panics on an invalid
    /// config.
    pub fn new(config: &str) -> Sim {
        let config = match load(config) {
            Ok(c) => c,
            Err(e) => panic!("invalid sim config: {e:?}"),
        };
        Sim {
            core: Core::new(config),
            now: Instant::from_nanos(0),
            next_id: 1,
            conns: BTreeMap::new(),
            logs: Vec::new(),
            trace: String::new(),
            actions: Vec::new(),
            pending_read: None,
            step_limit: STEP_LIMIT,
            fs: Fs::default(),
            disk_delay: Duration::ZERO,
            disk: VecDeque::new(),
            files: BTreeMap::new(),
            context: None,
        }
    }

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

    /// The core's static roots.
    pub fn static_roots(&self) -> &[PathBuf] {
        self.core.static_roots()
    }

    /// A new client connection from `peer`; delivers `Accepted` at once.
    pub fn connect(&mut self, peer: IpAddr) -> ConnId {
        let conn = ConnId(self.next_id);
        self.next_id += 1;
        self.conns.insert(
            conn,
            SimConn {
                to_server: VecDeque::new(),
                eof: Eof::NotSent,
                client: ClientSide::Live,
                received: Vec::new(),
                read_chunk: usize::MAX,
                window: Window::Unlimited,
                interest: Interest::None,
                state: SimConnState::Open,
                send_capacity: 0,
            },
        );
        self.deliver(Event::Accepted { conn, peer });
        let send_capacity = self.core.send_capacity(conn);
        self.conn_mut(conn).send_capacity = send_capacity;
        conn
    }

    /// Queue `bytes` from the client to the server.
    pub fn send(&mut self, conn: ConnId, bytes: &[u8]) {
        let c = self.conn_mut(conn);
        assert_eq!(c.eof, Eof::NotSent, "send after send_eof on {conn:?}");
        c.to_server.extend(bytes);
    }

    /// Queue end of stream from the client, after any queued bytes.
    pub fn send_eof(&mut self, conn: ConnId) {
        let c = self.conn_mut(conn);
        if c.eof == Eof::NotSent {
            c.eof = Eof::Queued;
        }
    }

    /// The client resets the connection; delivers `Failed` at once (unless
    /// the server already closed it).
    pub fn reset(&mut self, conn: ConnId) {
        let c = self.conn_mut(conn);
        if c.client == ClientSide::Reset || c.state == SimConnState::Closed {
            return;
        }
        c.client = ClientSide::Reset;
        c.to_server.clear();
        self.deliver(Event::Failed {
            conn,
            error: ConnError::Reset,
        });
    }

    /// Deliver at most `max` bytes per `Received`.
    pub fn set_read_chunk(&mut self, conn: ConnId, max: usize) {
        assert!(max > 0, "read chunk must be positive");
        self.conn_mut(conn).read_chunk = max;
    }

    /// How many bytes the client accepts per `Sent`.
    pub fn set_client_window(&mut self, conn: ConnId, window: Window) {
        self.conn_mut(conn).window = window;
    }

    /// Pump queued bytes both ways until no connection makes progress.
    pub fn run_until_idle(&mut self) {
        let mut steps: usize = 0;
        while self.pump_once() {
            steps += 1;
            if steps > self.step_limit {
                self.violation(&format!(
                    "run_until_idle exceeded {} steps",
                    self.step_limit
                ));
            }
        }
        let stuck = self.conns.iter().find(|(id, c)| {
            c.state != SimConnState::Closed
                && c.interest != Interest::Write
                && (!self.core.send_buffer(**id).is_empty() || self.core.send_file(**id).is_some())
        });
        if let Some((conn, c)) = stuck {
            self.violation(&format!(
                "conn={} has pending send bytes but interest stays {:?}",
                conn.0, c.interest
            ));
        }
    }

    /// One pass over all connections; returns whether anything moved.
    fn pump_once(&mut self) -> bool {
        let mut progress = self.pump_disk();
        let ids: Vec<ConnId> = self.conns.keys().copied().collect();
        for conn in ids {
            let c = &self.conns[&conn];
            if c.state == SimConnState::Closed || c.client == ClientSide::Reset {
                continue;
            }
            match c.interest {
                Interest::None => {}
                Interest::Read => progress |= self.pump_read(conn),
                Interest::Write => progress |= self.pump_write(conn),
            }
        }
        progress
    }

    fn pump_read(&mut self, conn: ConnId) -> bool {
        let c = self.conns.get_mut(&conn).unwrap();
        if !c.to_server.is_empty() {
            return self.deliver_read(conn);
        }
        if c.eof == Eof::Queued {
            c.eof = Eof::Delivered;
            self.deliver(Event::ReadClosed { conn });
            return true;
        }
        false
    }

    /// The only path that writes into `recv_buffer`: copies as many queued
    /// client bytes as allowed and reports them with `Received` right away.
    fn deliver_read(&mut self, conn: ConnId) -> bool {
        let c = self.conns.get_mut(&conn).unwrap();
        let spare = self.core.recv_buffer(conn);
        let n = spare.len().min(c.read_chunk).min(c.to_server.len());
        if n == 0 {
            return false;
        }
        for (dst, src) in spare.iter_mut().zip(c.to_server.drain(..n)) {
            *dst = src;
        }
        self.pending_read = Some(conn);
        self.deliver(Event::Received { conn, len: n });
        true
    }

    /// Deliver every disk completion that is due.
    fn pump_disk(&mut self) -> bool {
        let mut progress = false;
        while self.disk.front().is_some_and(|(due, _)| *due <= self.now) {
            let (_, op) = self.disk.pop_front().unwrap();
            progress = true;
            self.complete(op);
        }
        progress
    }

    fn complete(&mut self, op: DiskOp) {
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

    fn window(&self, conn: ConnId) -> usize {
        match self.conn(conn).window {
            Window::Unlimited => usize::MAX,
            Window::Bytes(n) => n,
            Window::Stalled => 0,
            Window::Budget(n) => n,
        }
    }

    /// The client's window took `n` bytes.
    fn take_window(&mut self, conn: ConnId, n: usize) {
        let c = self.conn_mut(conn);
        if let Window::Budget(left) = c.window {
            c.window = Window::Budget(left - n);
        }
    }

    /// `sendfile` of `segment` to `conn`, as far as the window and the
    /// file's end allow.
    fn pump_sendfile(&mut self, conn: ConnId, segment: FileSegment) -> bool {
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

    fn pump_write(&mut self, conn: ConnId) -> bool {
        let window = self.window(conn);
        let c = self.conns.get_mut(&conn).unwrap();
        let pending = self.core.send_buffer(conn);
        if pending.is_empty() {
            return match self.core.send_file(conn) {
                Some(segment) => self.pump_sendfile(conn, segment),
                None => false,
            };
        }
        let n = pending.len().min(window);
        if n == 0 {
            return false;
        }
        if c.state == SimConnState::WriteShutdown {
            self.violation(&format!("conn={} writes after ShutdownWrite", conn.0));
        }
        if let Window::Budget(left) = c.window {
            c.window = Window::Budget(left - n);
        }
        c.received.extend_from_slice(&pending[..n]);
        self.deliver(Event::Sent { conn, len: n });
        true
    }

    /// Move the clock forward by `by`, firing every deadline and completing
    /// every disk operation on the way, in time order (disk first on a tie).
    pub fn advance(&mut self, by: Duration) {
        let target = self.now.saturating_add(by);
        loop {
            let timer = self.core.next_deadline();
            let disk = self.disk.front().map(|(due, _)| *due);
            let next = match (timer, disk) {
                (Some(t), Some(d)) => t.min(d),
                (Some(t), None) => t,
                (None, Some(d)) => d,
                (None, None) => break,
            };
            if next > target {
                break;
            }
            self.now = self.now.max(next);
            if disk.is_some_and(|d| d <= self.now) {
                self.run_until_idle();
                continue;
            }
            self.deliver(Event::TimerFired);
            if let Some(next) = self.core.next_deadline() {
                if next <= self.now {
                    self.violation(&format!(
                        "after TimerFired at t={}ns next_deadline is t={}ns, not later",
                        self.now.as_nanos(),
                        next.as_nanos()
                    ));
                }
            }
            self.run_until_idle();
        }
        self.now = target;
    }

    /// The simulated current time.
    pub fn now(&self) -> Instant {
        self.now
    }

    /// Take the bytes the client has received so far on `conn`.
    pub fn take_received(&mut self, conn: ConnId) -> Vec<u8> {
        std::mem::take(&mut self.conn_mut(conn).received)
    }

    /// Client bytes not yet delivered to the server.
    pub fn queued_to_server(&self, conn: ConnId) -> usize {
        self.conn(conn).to_server.len()
    }

    /// The interest the core last set for `conn`.
    pub fn interest(&self, conn: ConnId) -> Interest {
        self.conn(conn).interest
    }

    /// The network-side state of `conn`.
    pub fn state(&self, conn: ConnId) -> SimConnState {
        self.conn(conn).state
    }

    /// Every connection id the sim has handed out, in order.
    pub fn conn_ids(&self) -> Vec<ConnId> {
        self.conns.keys().copied().collect()
    }

    /// Whether the core still holds state for `conn` (it reports buffer
    /// capacity only for connections it knows).
    pub fn core_knows(&self, conn: ConnId) -> bool {
        self.core.send_capacity(conn) > 0 || self.core.recv_capacity(conn) > 0
    }

    /// Every log entry the core has emitted, in order.
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }

    /// One line per event and action, in order.
    pub fn trace(&self) -> &str {
        &self.trace
    }

    fn conn(&self, conn: ConnId) -> &SimConn {
        match self.conns.get(&conn) {
            Some(c) => c,
            None => panic!("unknown sim connection {conn:?}"),
        }
    }

    fn conn_mut(&mut self, conn: ConnId) -> &mut SimConn {
        match self.conns.get_mut(&conn) {
            Some(c) => c,
            None => panic!("unknown sim connection {conn:?}"),
        }
    }

    fn violation(&self, what: &str) -> ! {
        panic!("sim invariant violated: {what}\ntrace:\n{}", self.trace);
    }

    fn line(&mut self, text: &str) {
        let _ = writeln!(self.trace, "t={}ns {text}", self.now.as_nanos());
    }

    /// Hand one event to the core and apply the resulting actions.
    fn deliver(&mut self, event: Event) {
        self.line(&event_text(&event));
        if let Some(pending) = self.pending_read.take() {
            let matches = match &event {
                Event::Received { conn, .. } => *conn == pending,
                Event::Accepted { .. }
                | Event::ReadClosed { .. }
                | Event::Sent { .. }
                | Event::Failed { .. }
                | Event::TimerFired
                | Event::FileOpened { .. }
                | Event::FileFailed { .. }
                | Event::ChunkReady { .. }
                | Event::FileSent { .. } => false,
            };
            if !matches {
                self.violation(&format!(
                    "read into recv_buffer of conn={} not reported by its Received \
                     before the next event",
                    pending.0
                ));
            }
        }
        self.context = match &event {
            Event::Accepted { conn, .. }
            | Event::Received { conn, .. }
            | Event::ReadClosed { conn }
            | Event::Sent { conn, .. }
            | Event::Failed { conn, .. }
            | Event::FileSent { conn, .. } => Some(*conn),
            Event::FileOpened { file, .. }
            | Event::FileFailed { file, .. }
            | Event::ChunkReady { file, .. } => self.files.get(file).map(|t| t.conn),
            Event::TimerFired => None,
        };
        let mut actions = std::mem::take(&mut self.actions);
        actions.clear();
        self.core.handle(self.now, event, &mut actions);
        for action in actions.drain(..) {
            self.apply(action);
        }
        self.actions = actions;
        self.context = None;
        self.check_buffers();
        self.check_files();
    }

    /// Queue a disk completion, ordered by due time (after any already
    /// queued for the same instant).
    fn queue_disk(&mut self, due: Instant, op: DiskOp) {
        let at = self.disk.partition_point(|(d, _)| *d <= due);
        self.disk.insert(at, (due, op));
    }

    /// Perform a file action: start the disk operation or close the file.
    fn apply_file(&mut self, action: Action) {
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
            | Action::Log(_) => unreachable!("not a file action"),
        }
    }

    fn apply(&mut self, action: Action) {
        self.line(&action_text(&action));
        let conn = match &action {
            Action::SetInterest { conn, .. }
            | Action::ShutdownWrite { conn }
            | Action::Close { conn } => *conn,
            Action::Log(log) => log_conn(log),
            Action::OpenFile { .. } | Action::Readahead { .. } | Action::CloseFile { .. } => {
                return self.apply_file(action);
            }
        };
        let state = match self.conns.get(&conn) {
            Some(c) => c.state,
            None => self.violation(&format!("action for unknown conn={}", conn.0)),
        };
        if state == SimConnState::Closed {
            self.violation(&format!("action after Close on conn={}", conn.0));
        }
        let c = self.conns.get_mut(&conn).unwrap();
        match action {
            Action::SetInterest { interest, .. } => c.interest = interest,
            Action::ShutdownWrite { .. } => c.state = SimConnState::WriteShutdown,
            Action::Close { .. } => {
                c.state = SimConnState::Closed;
                c.interest = Interest::None;
            }
            Action::Log(log) => self.logs.push(log),
            Action::OpenFile { .. } | Action::Readahead { .. } | Action::CloseFile { .. } => {}
        }
    }

    /// No file stays open once its connection is closed, and a segment is
    /// offered only for an open file of that connection, within a range
    /// whose `ChunkReady` was delivered, while the send buffer is empty.
    fn check_files(&self) {
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

    fn check_buffers(&mut self) {
        let open: Vec<(ConnId, usize, Interest)> = self
            .conns
            .iter()
            .filter(|(_, c)| c.state != SimConnState::Closed && c.send_capacity > 0)
            .map(|(id, c)| (*id, c.send_capacity, c.interest))
            .collect();
        for (conn, send_cap, interest) in open {
            if self.core.send_capacity(conn) != send_cap {
                self.violation(&format!(
                    "conn={} send buffer capacity changed while open",
                    conn.0
                ));
            }
            let recv_cap = self.core.recv_capacity(conn);
            let recv = self.core.recv_buffer(conn).len();
            if recv > recv_cap {
                self.violation(&format!(
                    "conn={} recv_buffer {recv} bytes exceeds capacity {recv_cap}",
                    conn.0
                ));
            }
            if interest == Interest::Read && recv == 0 {
                self.violation(&format!(
                    "conn={} wants to read but recv_buffer is empty",
                    conn.0
                ));
            }
            let send = self.core.send_buffer(conn).len();
            if send > send_cap {
                self.violation(&format!(
                    "conn={} send_buffer {send} bytes exceeds capacity {send_cap}",
                    conn.0
                ));
            }
        }
    }
}

fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. } | Log::Rejected { conn, .. } | Log::Reset { conn, .. } => *conn,
    }
}

fn event_text(event: &Event) -> String {
    match event {
        Event::Accepted { conn, peer } => format!("conn={} Accepted peer={peer}", conn.0),
        Event::Received { conn, len } => format!("conn={} Received len={len}", conn.0),
        Event::ReadClosed { conn } => format!("conn={} ReadClosed", conn.0),
        Event::Sent { conn, len } => format!("conn={} Sent len={len}", conn.0),
        Event::Failed { conn, error } => format!("conn={} Failed error={error:?}", conn.0),
        Event::TimerFired => "TimerFired".to_owned(),
        Event::FileOpened { file, meta } => format!(
            "file={} FileOpened kind={:?} size={} modified_ns={}",
            file.0, meta.kind, meta.stamp.size, meta.stamp.modified_ns
        ),
        Event::FileFailed { file, error } => {
            format!("file={} FileFailed error={error:?}", file.0)
        }
        Event::ChunkReady { file, stamp } => format!(
            "file={} ChunkReady size={} modified_ns={}",
            file.0, stamp.size, stamp.modified_ns
        ),
        Event::FileSent { conn, len } => format!("conn={} FileSent len={len}", conn.0),
    }
}

fn action_text(action: &Action) -> String {
    match action {
        Action::SetInterest { conn, interest } => {
            format!("conn={} -> SetInterest {interest:?}", conn.0)
        }
        Action::ShutdownWrite { conn } => format!("conn={} -> ShutdownWrite", conn.0),
        Action::Close { conn } => format!("conn={} -> Close", conn.0),
        Action::Log(log) => format!("conn={} -> Log {log:?}", log_conn(log).0),
        Action::OpenFile { file, root, path } => {
            let path: Vec<String> = path
                .iter()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            format!("file={} -> OpenFile root={} path={path:?}", file.0, root.0)
        }
        Action::Readahead { file, offset, len } => {
            format!("file={} -> Readahead offset={offset} len={len}", file.0)
        }
        Action::CloseFile { file } => format!("file={} -> CloseFile", file.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const CONFIG: &str = "listen 127.0.0.1:8080; host a.test { path / { static /srv; } }";

    #[test]
    #[should_panic(expected = "read into recv_buffer of conn=1 not reported")]
    fn unreported_read_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
        // Simulate a driver bug: bytes written into conn 1's receive buffer,
        // then a different event handled before the matching `Received`.
        sim.pending_read = Some(c);
        sim.deliver(Event::TimerFired);
    }

    fn track(sim: &mut Sim, conn: ConnId, file: u64, phase: Phase) {
        sim.files.insert(
            FileId(file),
            FileTrack {
                conn,
                root: "/srv".to_owned(),
                key: Vec::new(),
                phase,
            },
        );
    }

    fn open(size: u64) -> Phase {
        Phase::Open {
            size,
            chunks: 0,
            ready: Vec::new(),
            handle: None,
        }
    }

    #[test]
    #[should_panic(expected = "second CloseFile for file=1")]
    fn second_close_file_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
        track(&mut sim, c, 1, open(5));
        sim.apply(Action::CloseFile { file: FileId(1) });
        sim.apply(Action::CloseFile { file: FileId(1) });
    }

    #[test]
    #[should_panic(expected = "Readahead for unknown or closed file=9")]
    fn action_for_unknown_file_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        sim.apply(Action::Readahead {
            file: FileId(9),
            offset: 0,
            len: 1,
        });
    }

    #[test]
    #[should_panic(expected = "beyond the size 5 seen at open")]
    fn readahead_beyond_size_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
        track(&mut sim, c, 1, open(5));
        sim.apply(Action::Readahead {
            file: FileId(1),
            offset: 2,
            len: 4,
        });
    }

    #[test]
    #[should_panic(expected = "file=1 left open after conn=1 closed")]
    fn file_left_open_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
        track(&mut sim, c, 1, open(5));
        sim.conn_mut(c).state = SimConnState::Closed;
        sim.check_files();
    }

    #[test]
    #[should_panic(expected = "send_file segment 0..5 of file=1 before its ChunkReady")]
    fn segment_before_chunk_ready_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        sim.add_file("/srv", "f", b"hello");
        sim.set_disk_delay(Duration::from_secs(1));
        let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
        sim.send(c, b"GET /f HTTP/1.1\r\nHost: a.test\r\n\r\n");
        sim.run_until_idle();
        // Open completes, then the head is written; the readahead is still
        // in flight when the core is handed its ChunkReady behind the sim's
        // back, so the sim never saw that range become ready.
        sim.advance(Duration::from_secs(1));
        sim.run_until_idle();
        let stamp = FileStamp {
            size: 5,
            modified_ns: 1,
        };
        let mut actions = Vec::new();
        let now = sim.now;
        sim.core.handle(
            now,
            Event::ChunkReady {
                file: FileId(1),
                stamp,
            },
            &mut actions,
        );
        sim.check_files();
    }

    #[test]
    #[should_panic(expected = "OpenFile file=1 outside a connection's event")]
    fn open_file_outside_a_connection_event_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        sim.apply(Action::OpenFile {
            file: FileId(1),
            root: RootId(0),
            path: Vec::new(),
        });
    }

    #[test]
    fn disk_queue_is_ordered_by_due_time() {
        let mut sim = Sim::new(CONFIG);
        let at = |n| Instant::from_nanos(n);
        for (due, file) in [(5, 1), (3, 2), (5, 3), (1, 4), (3, 5)] {
            sim.queue_disk(at(due), DiskOp::Open { file: FileId(file) });
        }
        let order: Vec<(u64, u64)> = sim
            .disk
            .iter()
            .map(|(due, op)| (due.as_nanos(), op.file().0))
            .collect();
        assert_eq!(order, [(1, 4), (3, 2), (3, 5), (5, 1), (5, 3)]);
    }

    #[test]
    #[should_panic(expected = "run_until_idle exceeded 5 steps")]
    fn runaway_pump_is_a_violation() {
        let mut sim = Sim::new(CONFIG);
        sim.step_limit = 5;
        let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
        sim.set_read_chunk(c, 1);
        sim.send(c, b"0123456789");
        sim.run_until_idle();
    }
}
