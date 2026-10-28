//! The epoll event loop: it runs the core over real sockets and files,
//! following the driver contract (`core/src/core/contract.rs`, DRV-01…10,
//! DRV-13, DRV-15, DRV-16) to the letter.
//!
//! One thread owns everything. Each turn waits on the epoll set with a
//! timeout derived from [`Core::next_deadline`], handles every ready
//! descriptor (listeners, clients, upstreams, the wake eventfd), then
//! delivers `TimerFired` if the deadline has passed. Every event goes
//! through `Driver::deliver`: one `handle` call whose actions are all
//! performed, in order, before anything else happens (DRV-02), followed by
//! the connect answers those actions queued (DRV-10: never inside the call
//! that emitted the `Connect`).
//!
//! Clients are in `clients.rs` (accept, reads, writes, their actions),
//! upstreams in `upstreams.rs`, the file contract (the disk pool's jobs
//! and results, `sendfile`) in `files.rs`.

mod accept;
mod clients;
mod files;
mod upstreams;

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use durchlauferhitzer_core::{
    Action, ConnError, ConnId, Core, Event, FileError, FileId, Instant, UpstreamId,
};

use self::accept::AcceptGate;
use self::files::Files;
use crate::conns::{Client, Token, Upstream};
use crate::log::{Buffered, LogEntry, LogSink};
use crate::pool::Pool;
use crate::sys::epoll::{Epoll, EpollEvent, Readiness};
use crate::sys::eventfd::EventFd;

/// How many ready descriptors one `epoll_wait` returns at most.
const EVENTS_PER_WAIT: usize = 256;

/// How [`Driver::run`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// A [`StopHandle`] asked the loop to stop. The connections still
    /// open are closed when the driver is dropped. (Task 9 adds the
    /// signal-driven endings: drained, aborted.)
    Stopped,
}

/// Asks a running [`Driver`] to stop: the loop returns
/// [`Exit::Stopped`] at its next turn. Cheap to clone; usable from any
/// thread.
#[derive(Debug, Clone)]
pub struct StopHandle {
    wake: Arc<Wake>,
}

impl StopHandle {
    /// Sets the stop request and wakes the loop.
    pub fn stop(&self) -> io::Result<()> {
        self.wake.stop.store(true, Ordering::SeqCst);
        self.wake.fd.notify()
    }
}

/// The eventfd that wakes the loop, with the stop request it may carry.
#[derive(Debug)]
struct Wake {
    fd: EventFd,
    stop: AtomicBool,
}

/// A socket whose epoll registration failed (DRV-15), with the error to
/// report: its `Failed` / `UpstreamFailed` is delivered after the actions
/// of the current call, unless the core closed it first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    Client(ConnId, ConnError),
    Upstream(UpstreamId, ConnError),
}

/// Whether the loop waits for the log descriptor to become writable
/// (FR-LOG-06): only while the sink holds bytes, so an idle loop is never
/// woken by an always-writable stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogWait {
    Idle,
    Writable,
}

/// A file request the driver answers `FileFailed { Other }` itself, after
/// the actions of the current call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileFailure {
    /// An `OpenFile`: always answered, since the core never closes a file
    /// whose open is unanswered.
    Open(FileId),
    /// A `Readahead`: answered only while the file is still open; after
    /// `CloseFile` nothing is delivered for it (DRV-09).
    Read(FileId),
}

/// The time sources: the monotonic clock as the core's [`Instant`] (DRV-06)
/// and the wall-clock second for `Date` (DRV-13).
#[derive(Debug)]
struct Clock {
    /// The core's instant 0.
    origin: std::time::Instant,
    /// The wall-clock second last passed to [`Core::set_date`].
    second: Option<u64>,
}

impl Clock {
    fn now(&self) -> Instant {
        let nanos = self.origin.elapsed().as_nanos();
        Instant::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
    }

    /// The wall-clock second if it differs from the last one returned. A
    /// clock before 1970 counts as second 0.
    fn changed_second(&mut self) -> Option<u64> {
        let second = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if self.second == Some(second) {
            return None;
        }
        self.second = Some(second);
        Some(second)
    }
}

/// The production driver: the core on epoll.
#[derive(Debug)]
pub struct Driver {
    core: Core,
    epoll: Epoll,
    wake: Arc<Wake>,
    listeners: Vec<OwnedFd>,
    /// Whether the listeners take connections (DRV-16).
    accepting: AcceptGate,
    /// The static roots, opened once at startup, indexed by `RootId`
    /// (DRV-07); each open job holds a reference while it runs.
    roots: Vec<Arc<OwnedFd>>,
    /// The disk worker pool (DRV-08); `None` exactly when the core has no
    /// static roots, and once `run` has ended (it shuts the pool down).
    pool: Option<Pool>,
    /// Every file id between its `OpenFile` and its `CloseFile`.
    files: Files,
    /// File requests answered `FileFailed { Other }` after the current
    /// call: an open for a root or pool the driver does not have (never,
    /// past `new`), or a second readahead while one is in flight (never,
    /// from the core).
    file_failures: VecDeque<FileFailure>,
    log: LogSink,
    log_wait: LogWait,
    clients: BTreeMap<ConnId, Client>,
    upstreams: BTreeMap<UpstreamId, Upstream>,
    /// Upstreams whose connect outcome waits to be delivered.
    answers: VecDeque<UpstreamId>,
    /// Sockets whose failed registration waits to be reported.
    broken: VecDeque<Broken>,
    /// The next connection id: from 1, never reused (DRV-01).
    next_conn: u64,
    clock: Clock,
    /// Reused for every `handle` call.
    actions: Vec<Action>,
}

impl Driver {
    /// A driver for `core`, accepting on `listeners` (bound, listening,
    /// non-blocking sockets), with the static roots opened at startup in
    /// [`Core::static_roots`] order (DRV-07), the number of disk workers
    /// (`limits.disk_threads`) and the log sink.
    ///
    /// The disk pool is started exactly when the core has static roots:
    /// every `OpenFile` gets an answer (DRV-09), and a static-less core
    /// gets no idle threads (its `disk_threads` is not used). The pool's
    /// workers wake the loop through a duplicate of its wake eventfd (R1):
    /// one more descriptor in the budget (NFR-RES-05). The workers are
    /// spawned here, so a signal mask must be set (`SignalFd::new`)
    /// before this call.
    ///
    /// Refused (`InvalidInput`): a root list that does not match
    /// [`Core::static_roots`] one to one, and zero threads for a core
    /// with static roots.
    pub fn new(
        core: Core,
        listeners: Vec<OwnedFd>,
        roots: Vec<OwnedFd>,
        disk_threads: u32,
        mut log: LogSink,
    ) -> io::Result<Driver> {
        if roots.len() != core.static_roots().len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "one opened root per static root of the core (DRV-07)",
            ));
        }
        log.set_backends(core.backends());
        let epoll = Epoll::new()?;
        let wake = Arc::new(Wake {
            fd: EventFd::new()?,
            stop: AtomicBool::new(false),
        });
        epoll.add(wake.fd.as_fd(), token(Token::Wake)?, Readiness::Read)?;
        for (i, listener) in listeners.iter().enumerate() {
            let i = u32::try_from(i).map_err(|_| too_many("listeners"))?;
            epoll.add(
                listener.as_fd(),
                token(Token::Listener(i))?,
                Readiness::Read,
            )?;
        }
        // Last, so nothing above can fail with threads running (a `Pool`
        // dropped on an error path joins its threads anyway).
        let pool = if roots.is_empty() {
            None
        } else {
            Some(Pool::start(disk_threads, wake.fd.try_clone()?)?)
        };
        Ok(Driver {
            core,
            epoll,
            wake,
            listeners,
            accepting: AcceptGate::new(),
            roots: roots.into_iter().map(Arc::new).collect(),
            pool,
            files: Files::new(),
            file_failures: VecDeque::new(),
            log,
            log_wait: LogWait::Idle,
            clients: BTreeMap::new(),
            upstreams: BTreeMap::new(),
            answers: VecDeque::new(),
            broken: VecDeque::new(),
            next_conn: 1,
            clock: Clock {
                origin: std::time::Instant::now(),
                second: None,
            },
            actions: Vec::new(),
        })
    }

    /// A handle that stops [`Driver::run`] from another thread.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            wake: Arc::clone(&self.wake),
        }
    }

    /// Runs the loop until it is asked to stop. An `Err` is a failure of
    /// the loop itself (`epoll_wait`, or `epoll_ctl` on the listeners or
    /// the wake eventfd), never of one socket (DRV-15). Either way the disk
    /// pool is shut down (its threads joined) before `run` returns (R6);
    /// opens still with it are never answered, since the loop has ended.
    pub fn run(&mut self) -> io::Result<Exit> {
        let exit = self.run_loop();
        if let Some(pool) = self.pool.take() {
            pool.shutdown();
        }
        exit
    }

    fn run_loop(&mut self) -> io::Result<Exit> {
        let mut events = vec![EpollEvent::EMPTY; EVENTS_PER_WAIT];
        loop {
            if self.wake.stop.load(Ordering::SeqCst) {
                return Ok(Exit::Stopped);
            }
            self.sync_log();
            let timeout = self.timeout();
            let n = self.epoll.wait(&mut events, timeout)?;
            for event in events.iter().take(n) {
                self.handle_ready(event)?;
            }
            self.fire_timer()?;
            self.retry_accept()?;
        }
    }

    /// How long to wait: until [`Core::next_deadline`] (DRV-06) or the
    /// paused listeners' retry (DRV-16), whichever comes first; forever
    /// without either. `Epoll::wait` rounds up to whole milliseconds, so
    /// the loop never wakes before the deadline.
    fn timeout(&self) -> Option<Duration> {
        let deadline = match (self.core.next_deadline(), self.accepting.retry_at()) {
            (Some(core), Some(retry)) => core.min(retry),
            (Some(deadline), None) | (None, Some(deadline)) => deadline,
            (None, None) => return None,
        };
        let now = self.clock.now();
        Some(Duration::from_nanos(
            deadline.as_nanos().saturating_sub(now.as_nanos()),
        ))
    }

    /// Delivers `TimerFired` once the deadline has passed (DRV-06).
    fn fire_timer(&mut self) -> io::Result<()> {
        match self.core.next_deadline() {
            Some(deadline) if deadline <= self.clock.now() => self.deliver(Event::TimerFired),
            Some(_) | None => Ok(()),
        }
    }

    /// One ready descriptor: its I/O and events, then whatever that left
    /// queued. A registration change outside any `deliver` (a write that
    /// fails under `ReadWrite` stops writing) can queue a failure that no
    /// later call of this descriptor would deliver (DRV-15).
    fn handle_ready(&mut self, event: &EpollEvent) -> io::Result<()> {
        self.dispatch(event)?;
        self.flush_queued()
    }

    fn dispatch(&mut self, event: &EpollEvent) -> io::Result<()> {
        // A value no token encodes to cannot come from our registrations.
        let Some(token) = Token::decode(event.token()) else {
            return Ok(());
        };
        match token {
            Token::Listener(i) => self.accept_all(i),
            Token::Client(conn) => self.client_event(conn, event.readiness(), event.trouble()),
            Token::Upstream(up) => self.upstream_event(up, event.readiness(), event.trouble()),
            Token::Wake => {
                // The stop request is read at the top of the loop. The
                // counter is reset before the results are taken, so a
                // result pushed after this drain wakes the loop again.
                self.wake.fd.drain()?;
                self.pool_results()
            }
            // Not registered before Task 9.
            Token::Signal => Ok(()),
            // Writable, or an error the next write reports (FR-LOG-06).
            // `event.trouble()` is not looked at: on `EPOLLERR`/`EPOLLHUP`
            // this flush gets `EPIPE`/`EIO` and discards the buffer, which
            // ends the wait (`sync_log`). So the flush must not be
            // skipped on trouble, or the loop would spin on it.
            Token::Log => {
                self.log.flush();
                Ok(())
            }
        }
    }

    /// Registers the log descriptor for writability while the sink holds
    /// bytes, and takes it out of the set once it holds none (FR-LOG-06).
    /// The log never ends the loop: if the registration fails (`EPERM`
    /// for a descriptor the kernel cannot poll, such as a regular file or
    /// `/dev/null`, whose writes never return `EAGAIN` anyway; `ENOMEM`,
    /// `ENOSPC`), the bytes wait for the next entry's write and the
    /// registration is tried again next turn. A failed removal leaves
    /// nothing registered that the loop knows of (`ENOENT`), so it counts
    /// as done.
    fn sync_log(&mut self) {
        let Some(fd) = self.log.out_fd() else {
            return;
        };
        match (self.log.buffered(), self.log_wait) {
            (Buffered::Bytes, LogWait::Idle) => {
                let added =
                    token(Token::Log).and_then(|token| self.epoll.add(fd, token, Readiness::Write));
                if added.is_ok() {
                    self.log_wait = LogWait::Writable;
                }
            }
            (Buffered::Nothing, LogWait::Writable) => {
                let _ = self.epoll.delete(fd);
                self.log_wait = LogWait::Idle;
            }
            (Buffered::Bytes, LogWait::Writable) | (Buffered::Nothing, LogWait::Idle) => {}
        }
    }

    /// One event into the core (DRV-02): its actions are performed in
    /// order; then every connect answer they queued is delivered, each as
    /// its own later event (DRV-10), and every socket failure their
    /// registrations hit (DRV-15).
    fn deliver(&mut self, event: Event) -> io::Result<()> {
        self.call(event)?;
        self.flush_queued()
    }

    /// Delivers the queued connect answers, socket failures and failed
    /// opens, each as its own event, until none is left.
    fn flush_queued(&mut self) -> io::Result<()> {
        loop {
            if let Some(up) = self.answers.pop_front() {
                if let Some(answer) = self.take_answer(up)? {
                    self.call(answer)?;
                }
            } else if let Some(broken) = self.broken.pop_front() {
                if let Some(failed) = self.broken_event(broken) {
                    self.call(failed)?;
                }
            } else if let Some(failure) = self.file_failures.pop_front() {
                if let Some(failed) = self.failure_event(failure) {
                    self.call(failed)?;
                }
            } else {
                return Ok(());
            }
        }
    }

    /// The answer to a file request the driver failed itself, unless the
    /// file was closed meanwhile (a `Read` after `CloseFile`, DRV-09).
    fn failure_event(&self, failure: FileFailure) -> Option<Event> {
        let failed = |file| Event::FileFailed {
            file,
            error: FileError::Other,
        };
        match failure {
            FileFailure::Open(file) => Some(failed(file)),
            FileFailure::Read(file) => self.files.fd(file).is_some().then(|| failed(file)),
        }
    }

    /// The failure to report for a socket whose registration failed
    /// (DRV-15), unless the core closed it meanwhile (DRV-05).
    fn broken_event(&self, broken: Broken) -> Option<Event> {
        match broken {
            Broken::Client(conn, error) => self
                .clients
                .contains_key(&conn)
                .then_some(Event::Failed { conn, error }),
            Broken::Upstream(up, error) => self
                .upstreams
                .contains_key(&up)
                .then_some(Event::UpstreamFailed { up, error }),
        }
    }

    /// One `handle` call and its actions. The wall-clock second is passed
    /// on first whenever it changed (DRV-13).
    fn call(&mut self, event: Event) -> io::Result<()> {
        if let Some(second) = self.clock.changed_second() {
            self.core.set_date(second);
        }
        let now = self.clock.now();
        let mut actions = std::mem::take(&mut self.actions);
        actions.clear();
        self.core.handle(now, event, &mut actions);
        let mut result = Ok(());
        for action in actions.drain(..) {
            if result.is_ok() {
                result = self.apply(action);
            }
        }
        self.actions = actions;
        result
    }

    /// Performs one action. Every variant is matched, so a new action is a
    /// compile error here.
    fn apply(&mut self, action: Action) -> io::Result<()> {
        match action {
            Action::SetInterest { conn, interest } => self.set_interest(conn, interest),
            Action::ShutdownWrite { conn } => {
                self.shutdown_write(conn);
                Ok(())
            }
            Action::Close { conn } => self.close(conn),
            Action::OpenFile { file, root, path } => {
                self.open_file(file, root, path);
                Ok(())
            }
            Action::Readahead { file, offset, len } => {
                self.readahead(file, offset, len);
                Ok(())
            }
            Action::CloseFile { file } => self.close_file(file),
            Action::Connect { up, backend } => self.connect(up, backend),
            Action::SetUpstreamInterest { up, interest } => {
                self.set_upstream_interest(up, interest)
            }
            Action::CloseUpstream { up } => self.close_upstream(up),
            Action::Log(log) => {
                self.log.write(LogEntry::Core(log));
                Ok(())
            }
        }
    }
}

/// A token's epoll value, or an error for an id beyond the token space.
fn token(token: Token) -> io::Result<u64> {
    token.encode().ok_or_else(|| too_many("ids"))
}

fn too_many(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("too many {what} for the epoll token space"),
    )
}

/// The contract's mapping of a socket error (DRV-09, DRV-10): `ECONNRESET`
/// and `EPIPE` are `Reset`, anything else `Other`.
fn conn_error(err: &io::Error) -> ConnError {
    match err.raw_os_error() {
        Some(libc::ECONNRESET | libc::EPIPE) => ConnError::Reset,
        Some(_) | None => ConnError::Other,
    }
}

#[cfg(test)]
mod tests;
