//! The disk worker pool (A10, DRV-08, DRV-09): `disk_threads` OS threads
//! that open files beneath a static root (`openat2`, then `fstat`) and read
//! chunks into the page cache (`readahead`, then `fstat` for the stamp), so
//! the event loop never blocks on the disk.
//!
//! Jobs go into one `Mutex<VecDeque>` with a `Condvar`; each result goes
//! into a channel, followed by one `notify` of the loop's wake eventfd
//! (a duplicate made with [`EventFd::try_clone`], R1). The loop drains
//! [`Pool::results`] whenever that eventfd is readable; results come in
//! any order (DRV-08).
//!
//! Rules:
//!
//! - Every job gets exactly one [`JobResult`]: a failure is a result like
//!   any other, and no worker panics on an I/O error (R6).
//! - A job holds its descriptor (`Arc<OwnedFd>`) only while it runs: the
//!   worker drops the job before it sends the result, so once the loop
//!   holds the result it holds the last reference the job had (DRV-09:
//!   after `CloseFile` the loop closes the descriptor only when the
//!   in-flight readahead's result arrives).
//! - The queue is not bounded here: the core asks for at most one open or
//!   readahead per connection at a time, orphaned opens included, so at
//!   most `max_connections` jobs are queued or running (NFR-RES-05).
//! - [`Pool::shutdown`] (also on drop) stops the workers and joins them,
//!   waiting at most 1 s: a worker still running then (stuck in the
//!   kernel) is detached and counted. A job a worker is running finishes
//!   first; queued jobs are dropped without a result, since the loop that
//!   wanted them has ended.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use durchlauferhitzer_core::FileId;

use crate::sys::eventfd::EventFd;
use crate::sys::file::{Stat, fstat, openat2_beneath, readahead};

/// Work for a disk worker.
#[derive(Debug)]
pub enum Job {
    /// Open `segments` beneath the static root `root_fd` (DRV-07/08; an
    /// empty list is the root itself), then `fstat` the descriptor.
    Open {
        file: FileId,
        root_fd: Arc<OwnedFd>,
        segments: Vec<Vec<u8>>,
    },
    /// Read `len` bytes at `offset` of the open `fd` into the page cache,
    /// then `fstat` it for the stamp at the time of the read.
    Readahead {
        file: FileId,
        fd: Arc<OwnedFd>,
        offset: u64,
        len: u64,
    },
}

/// The outcome of one [`Job`]: exactly one per job.
#[derive(Debug)]
pub enum JobResult {
    /// The open and its `fstat` succeeded.
    Opened {
        file: FileId,
        fd: OwnedFd,
        stat: Stat,
    },
    /// The open (or the `fstat` after it) failed; nothing is left open.
    OpenFailed { file: FileId, error: io::Error },
    /// The readahead and its `fstat` succeeded; `stat` is the stamp of the
    /// descriptor when the chunk was read.
    ChunkRead { file: FileId, stat: Stat },
    /// The readahead (or the `fstat` after it) failed.
    ReadFailed { file: FileId, error: io::Error },
}

#[cfg(test)]
impl JobResult {
    /// The file the job was for.
    pub fn file(&self) -> FileId {
        match self {
            JobResult::Opened { file, .. }
            | JobResult::OpenFailed { file, .. }
            | JobResult::ChunkRead { file, .. }
            | JobResult::ReadFailed { file, .. } => *file,
        }
    }
}

/// Whether the workers keep taking jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    Running,
    Stopping,
}

#[derive(Debug)]
struct Queue {
    jobs: VecDeque<Job>,
    run: Run,
}

/// What the pool and its workers share.
#[derive(Debug)]
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled when a job arrives or the pool stops.
    ready: Condvar,
}

impl Shared {
    /// The queue, also after a thread panicked while holding it: the queue
    /// itself is always consistent (every change is one push or pop).
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The next job, waiting for one; `None` once the pool stops.
    fn next(&self) -> Option<Job> {
        let mut queue = self.lock();
        loop {
            match queue.run {
                Run::Stopping => return None,
                Run::Running => {
                    if let Some(job) = queue.jobs.pop_front() {
                        return Some(job);
                    }
                    queue = self
                        .ready
                        .wait(queue)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            }
        }
    }
}

/// The disk worker pool.
#[derive(Debug)]
pub struct Pool {
    shared: Arc<Shared>,
    results: Receiver<JobResult>,
    threads: Vec<JoinHandle<()>>,
}

impl Pool {
    /// Starts `threads` workers (`disk_threads`, 1–64 by the config) that
    /// `notify` `wake` after every result. Zero threads is `InvalidInput`;
    /// a thread that cannot be spawned stops the ones already started and
    /// returns the error.
    pub fn start(threads: u32, wake: EventFd) -> io::Result<Pool> {
        if threads == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the disk pool needs at least one thread",
            ));
        }
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: VecDeque::new(),
                run: Run::Running,
            }),
            ready: Condvar::new(),
        });
        let (sender, results) = mpsc::channel();
        let wake = Arc::new(wake);
        let mut pool = Pool {
            shared,
            results,
            threads: Vec::new(),
        };
        for i in 0..threads {
            let shared = Arc::clone(&pool.shared);
            let sender = sender.clone();
            let wake = Arc::clone(&wake);
            let spawned = thread::Builder::new()
                .name(format!("disk-{i}"))
                .spawn(move || work(&shared, &sender, &wake));
            match spawned {
                Ok(handle) => pool.threads.push(handle),
                Err(err) => {
                    pool.stop();
                    return Err(err);
                }
            }
        }
        Ok(pool)
    }

    /// Queues a job; a worker takes it as soon as one is free. A job
    /// submitted after [`Pool::shutdown`] began is never run.
    pub fn submit(&self, job: Job) {
        let mut queue = self.shared.lock();
        match queue.run {
            Run::Running => {
                queue.jobs.push_back(job);
                drop(queue);
                self.shared.ready.notify_one();
            }
            Run::Stopping => {}
        }
    }

    /// The results that are in, without waiting: drain it whenever the
    /// wake eventfd fires.
    pub fn results(&self) -> impl Iterator<Item = JobResult> + '_ {
        self.results.try_iter()
    }

    /// Stops the workers (R6, R3): a worker in the middle of a job
    /// finishes it first, queued jobs are dropped. It waits at most
    /// [`GRACE`] for the workers, joins those that ended and detaches the
    /// rest (a worker stuck in the kernel, a hung disk say): the process
    /// exits around them. Returns how many were detached, for the log.
    pub fn shutdown(mut self) -> usize {
        self.stop().detached
    }

    fn stop(&mut self) -> Stopped {
        self.stop_within(GRACE)
    }

    /// Stops the workers, waiting at most `grace` for them to end. The
    /// wait polls each thread's `is_finished` every [`POLL`]: std has no
    /// timed join, and this runs once, at exit.
    fn stop_within(&mut self, grace: Duration) -> Stopped {
        {
            let mut queue = self.shared.lock();
            queue.run = Run::Stopping;
            queue.jobs.clear();
        }
        self.shared.ready.notify_all();
        let deadline = Instant::now() + grace;
        while !self.threads.iter().all(JoinHandle::is_finished) && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        let mut stopped = Stopped {
            panicked: 0,
            detached: 0,
        };
        for handle in self.threads.drain(..) {
            if handle.is_finished() {
                if handle.join().is_err() {
                    stopped.panicked += 1;
                }
            } else {
                // Dropping the handle detaches the thread.
                stopped.detached += 1;
            }
        }
        stopped
    }
}

/// How long [`Pool::shutdown`] waits for the workers at most (R3).
const GRACE: Duration = Duration::from_secs(1);

/// How often the shutdown looks whether the workers have ended.
const POLL: Duration = Duration::from_millis(1);

/// How the workers ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stopped {
    /// Joined, having panicked (never, R6: counted for the tests).
    panicked: usize,
    /// Still running after the grace, and detached.
    detached: usize,
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A worker: runs jobs until the pool stops. The job (and the descriptor
/// it holds) is dropped before its result is sent.
fn work(shared: &Shared, results: &Sender<JobResult>, wake: &EventFd) {
    while let Some(job) = shared.next() {
        let result = run(job);
        if results.send(result).is_err() {
            // The pool is gone: nobody wants results any more.
            return;
        }
        // `notify` fails only for a broken eventfd, and there is nobody
        // to tell: the loop would not see the result until its next wake.
        let _ = wake.notify();
    }
}

/// Runs one job to its one result.
fn run(job: Job) -> JobResult {
    match job {
        Job::Open {
            file,
            root_fd,
            segments,
        } => match openat2_beneath(root_fd.as_fd(), &segments) {
            Ok(fd) => match fstat(fd.as_fd()) {
                Ok(stat) => JobResult::Opened { file, fd, stat },
                // `fd` closes here, on the worker: net zero descriptors, so
                // the loop has nothing to resume; paused listeners (DRV-16)
                // still come back by the 1 s retry.
                Err(error) => JobResult::OpenFailed { file, error },
            },
            Err(error) => JobResult::OpenFailed { file, error },
        },
        Job::Readahead {
            file,
            fd,
            offset,
            len,
        } => match readahead(fd.as_fd(), offset, len).and_then(|()| fstat(fd.as_fd())) {
            Ok(stat) => JobResult::ChunkRead { file, stat },
            Err(error) => JobResult::ReadFailed { file, error },
        },
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::fd::{AsFd, OwnedFd};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use durchlauferhitzer_core::{FileId, FileKind};

    use super::{GRACE, Job, JobResult, Pool, Stopped};
    use crate::sys::eventfd::EventFd;
    use crate::sys::file::open_dir_path;

    /// A root with `a.txt` (5 bytes) and `dir/`, removed on drop.
    struct Root(PathBuf);

    impl Root {
        fn new(tag: &str) -> (Root, Arc<OwnedFd>) {
            let path = std::env::temp_dir().join(format!("dlh-pool-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("dir")).unwrap();
            fs::write(path.join("a.txt"), b"hello").unwrap();
            let fd = Arc::new(open_dir_path(&path).unwrap());
            (Root(path), fd)
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn open(file: u64, root: &Arc<OwnedFd>, path: &[&[u8]]) -> Job {
        Job::Open {
            file: FileId(file),
            root_fd: Arc::clone(root),
            segments: path.iter().map(|s| s.to_vec()).collect(),
        }
    }

    /// Waits on the eventfd until `n` results are in (or 5 s pass).
    fn collect(pool: &Pool, wake: &EventFd, n: usize) -> Vec<JobResult> {
        let start = Instant::now();
        let mut out = Vec::new();
        while out.len() < n && start.elapsed() < Duration::from_secs(5) {
            let _ = wake.drain().unwrap();
            out.extend(pool.results());
            std::thread::sleep(Duration::from_millis(1));
        }
        out
    }

    #[test]
    fn zero_threads_is_refused() {
        let wake = EventFd::new().unwrap();
        let err = Pool::start(0, wake).err().unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// A10: every job gets exactly one result, whatever its outcome, and
    /// the workers wake the loop's eventfd for them.
    #[test]
    fn every_job_gets_exactly_one_result() {
        let (_root, root_fd) = Root::new("one");
        let wake = EventFd::new().unwrap();
        let pool = Pool::start(3, wake.try_clone().unwrap()).unwrap();
        let mut jobs = 0u64;
        for _ in 0..20 {
            jobs += 1;
            pool.submit(open(jobs, &root_fd, &[b"a.txt"]));
            jobs += 1;
            pool.submit(open(jobs, &root_fd, &[b"missing"]));
            jobs += 1;
            pool.submit(open(jobs, &root_fd, &[]));
            jobs += 1;
            // A NUL byte: refused before any system call, still answered.
            pool.submit(open(jobs, &root_fd, &[b"a\0b"]));
        }
        let results = collect(&pool, &wake, usize::try_from(jobs).unwrap());
        let mut seen = BTreeMap::new();
        for result in &results {
            *seen.entry(result.file()).or_insert(0) += 1;
            match result {
                JobResult::Opened { file, stat, .. } => {
                    let kind = if file.0 % 4 == 3 {
                        FileKind::Directory
                    } else {
                        FileKind::Regular
                    };
                    assert_eq!(stat.kind, kind, "{file:?}");
                    if kind == FileKind::Regular {
                        assert_eq!(stat.size, 5);
                    }
                }
                JobResult::OpenFailed { file, error } => {
                    let errno = if file.0 % 4 == 2 {
                        Some(libc::ENOENT)
                    } else {
                        None
                    };
                    assert_eq!(error.raw_os_error(), errno, "{file:?}");
                    assert!(file.0 % 4 == 2 || file.0 % 4 == 0, "{file:?}");
                }
                JobResult::ChunkRead { .. } | JobResult::ReadFailed { .. } => {
                    panic!("no readahead was submitted: {result:?}")
                }
            }
        }
        assert_eq!(seen.len(), usize::try_from(jobs).unwrap());
        assert!(seen.values().all(|&n| n == 1), "{seen:?}");
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(pool.results().count(), 0, "an extra result");
        pool.shutdown();
    }

    /// A readahead reports the stamp of the open descriptor; a readahead
    /// the kernel refuses (a directory) is a result too, never a panic.
    #[test]
    fn readahead_reports_the_stamp_or_its_error() {
        let (_root, root_fd) = Root::new("read");
        let wake = EventFd::new().unwrap();
        let pool = Pool::start(1, wake.try_clone().unwrap()).unwrap();
        pool.submit(open(1, &root_fd, &[b"a.txt"]));
        let [JobResult::Opened { fd, stat, .. }] = &collect(&pool, &wake, 1)[..] else {
            panic!("no open");
        };
        let opened = *stat;
        let fd = Arc::new(fd.as_fd().try_clone_to_owned().unwrap());
        pool.submit(Job::Readahead {
            file: FileId(1),
            fd: Arc::clone(&fd),
            offset: 0,
            len: 5,
        });
        pool.submit(Job::Readahead {
            file: FileId(2),
            fd: Arc::clone(&root_fd),
            offset: 0,
            len: 5,
        });
        let results = collect(&pool, &wake, 2);
        assert_eq!(results.len(), 2, "{results:?}");
        for result in results {
            match result {
                JobResult::ChunkRead { file, stat } => {
                    assert_eq!(file, FileId(1));
                    assert_eq!(stat, opened);
                }
                JobResult::ReadFailed { file, .. } => assert_eq!(file, FileId(2)),
                JobResult::Opened { .. } | JobResult::OpenFailed { .. } => {
                    panic!("{result:?}")
                }
            }
        }
        pool.shutdown();
    }

    /// R6: `shutdown` joins every worker; none panicked, and the queue's
    /// owners are gone with them. Jobs still queued are dropped.
    #[test]
    fn shutdown_joins_every_worker() {
        let (_root, root_fd) = Root::new("join");
        let wake = EventFd::new().unwrap();
        let mut pool = Pool::start(4, wake).unwrap();
        for file in 1..=100 {
            pool.submit(open(file, &root_fd, &[b"a.txt"]));
        }
        let shared = Arc::clone(&pool.shared);
        // The pool's, ours, and one per worker.
        assert_eq!(Arc::strong_count(&shared), 6);
        let stopped = pool.stop();
        assert_eq!(
            stopped,
            Stopped {
                panicked: 0,
                detached: 0
            }
        );
        assert_eq!(Arc::strong_count(&shared), 2, "a worker still runs");
        drop(pool);
    }

    /// R3: a worker stuck in the kernel (here a thread that blocks far
    /// longer) holds the shutdown for at most its grace, then is detached
    /// and counted; the others are joined. Production waits [`GRACE`],
    /// 1 s; here 200 ms, with 500 ms of margin for CI.
    #[test]
    fn shutdown_detaches_a_stuck_worker_after_its_grace() {
        assert_eq!(GRACE, Duration::from_secs(1));
        let wake = EventFd::new().unwrap();
        let mut pool = Pool::start(2, wake).unwrap();
        let (release, stuck) = std::sync::mpsc::channel::<()>();
        pool.threads.push(std::thread::spawn(move || {
            let _ = stuck.recv_timeout(Duration::from_secs(10));
        }));
        let grace = Duration::from_millis(200);
        let start = Instant::now();
        let stopped = pool.stop_within(grace);
        let elapsed = start.elapsed();
        assert_eq!(
            stopped,
            Stopped {
                panicked: 0,
                detached: 1
            }
        );
        assert!(
            elapsed >= grace && elapsed < grace + Duration::from_millis(500),
            "{elapsed:?}"
        );
        assert!(pool.threads.is_empty());
        drop(release);
    }

    /// R6: results nobody takes (the loop ended) never make a worker
    /// panic; dropping the pool shuts it down as well.
    #[test]
    fn dropping_the_pool_with_results_pending_is_quiet() {
        let (_root, root_fd) = Root::new("drop");
        let wake = EventFd::new().unwrap();
        let pool = Pool::start(2, wake).unwrap();
        for file in 1..=50 {
            pool.submit(open(file, &root_fd, &[b"a.txt"]));
        }
        drop(pool);
    }
}
