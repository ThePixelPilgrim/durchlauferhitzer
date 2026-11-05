//! The real allow-list (`containment::filter::program`, `fail`) installed
//! in a child process: the child is this binary again, told by an
//! environment variable what to do under the filter (no harness, no new
//! `unsafe`).
//!
//! - `abort`: an abort still ends the process by `SIGABRT` under the
//!   filter (glibc's path: `getpid`, `gettid`, `tgkill`; musl's: `tkill`
//!   of the main thread). The fatal-path
//!   group's `rt_sigaction` and `rt_sigreturn` (std's fault handlers) are
//!   listed but not exercised here.
//! - `fstat`: `sys::file::fstat` works under the filter (it calls the
//!   `fstat` system call itself, never `newfstatat`).
//! - `threads` (under `kill`): std's thread start, a join that waits for
//!   a thread still running (glibc's shared `futex` wait on its id;
//!   musl's shared wait and wake of its thread list lock), a
//!   timed condition-variable wait and a channel pass the `futex` check.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test binary; clippy.toml allows these in tests"
)]

use std::env;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitCode, ExitStatus};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use durchlauferhitzer::containment::filter::{Arch, program};
use durchlauferhitzer::sys::file::fstat;
use durchlauferhitzer::sys::seccomp::{Support, install, supported};
use durchlauferhitzer_core::FileKind;
use durchlauferhitzer_core::config::SeccompAction;

const CHILD: &str = "DURCHLAUFERHITZER_SECCOMP_CHILD";

/// What a child does under the filter.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Abort,
    Fstat,
    Threads,
}

impl Mode {
    const fn name(self) -> &'static str {
        match self {
            Self::Abort => "abort",
            Self::Fstat => "fstat",
            Self::Threads => "threads",
        }
    }
}

fn filter() {
    filter_with(SeccompAction::Fail);
}

fn filter_with(action: SeccompAction) {
    let arch = Arch::current().expect("a filtered architecture");
    install(&program(arch, action, std::process::id())).expect("install");
}

/// The child: installs the filter, then runs `mode`.
fn child(mode: Mode) -> ExitCode {
    match mode {
        Mode::Abort => {
            filter();
            std::process::abort();
        }
        Mode::Threads => {
            filter_with(SeccompAction::Kill);
            // Joined while it still sleeps: `pthread_join` waits on the
            // thread id.
            let sleeper = thread::spawn(|| thread::sleep(Duration::from_millis(100)));
            sleeper.join().unwrap();
            let pair = Arc::new((Mutex::new(false), Condvar::new()));
            let (lock, ready) = &*pair;
            let guard = lock.lock().unwrap();
            let (guard, timeout) = ready
                .wait_timeout(guard, Duration::from_millis(10))
                .unwrap();
            drop(guard);
            let (send, recv) = mpsc::channel();
            let sender = thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                send.send(7_u8).unwrap();
            });
            let got = recv.recv().unwrap();
            sender.join().unwrap();
            if timeout.timed_out() && got == 7 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Mode::Fstat => {
            // Opened and measured by std before the filter (`openat`,
            // `statx` are not listed).
            let file = File::open(env::current_exe().unwrap()).unwrap();
            let meta = file.metadata().unwrap();
            filter();
            let stat = match fstat(file.as_fd()) {
                Ok(stat) => stat,
                Err(err) => {
                    eprintln!("fstat under the filter: {err}");
                    return ExitCode::FAILURE;
                }
            };
            let mtime = u64::try_from(meta.mtime()).unwrap() * 1_000_000_000
                + u64::try_from(meta.mtime_nsec()).unwrap();
            if stat.kind == FileKind::Regular && stat.size == meta.len() && stat.mtime_ns == mtime {
                ExitCode::SUCCESS
            } else {
                eprintln!("fstat under the filter: {stat:?}, std: {meta:?}");
                ExitCode::FAILURE
            }
        }
    }
}

/// Runs this binary as a child in `mode`; kills it after 30 s.
fn spawn(mode: Mode) -> ExitStatus {
    let mut child = Command::new(env::current_exe().unwrap())
        .env(CHILD, mode.name())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > Duration::from_secs(30) {
            child.kill().unwrap();
            return child.wait().unwrap();
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn check(mode: Mode) -> Result<(), String> {
    let status = spawn(mode);
    let ok = match mode {
        Mode::Abort => status.signal() == Some(libc::SIGABRT),
        Mode::Fstat | Mode::Threads => status.success(),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("child: {status:?}"))
    }
}

fn main() -> ExitCode {
    match env::var(CHILD).as_deref() {
        Ok("abort") => return child(Mode::Abort),
        Ok("fstat") => return child(Mode::Fstat),
        Ok("threads") => return child(Mode::Threads),
        Ok(other) => panic!("unknown child mode {other}"),
        Err(_) => {}
    }
    if supported() != Support::Supported {
        println!("test seccomp_program ... FAILED: supported() is not Supported");
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for (name, mode) in [
        ("an_abort_dies_by_sigabrt_under_the_filter", Mode::Abort),
        ("fstat_works_under_the_filter", Mode::Fstat),
        ("threads_join_wait_and_wake_under_kill", Mode::Threads),
    ] {
        match check(mode) {
            Ok(()) => println!("test {name} ... ok"),
            Err(err) => {
                println!("test {name} ... FAILED: {err}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
