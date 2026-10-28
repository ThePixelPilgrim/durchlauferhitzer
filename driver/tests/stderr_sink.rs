//! `LogSink::stderr()` on the real fd 2 (FR-LOG-06), in a subprocess: this
//! test binary runs itself again with `DURCHLAUFERHITZER_SINK_CHILD=1`,
//! and in that child [`stderr_sink_child`] creates the sink on its stderr
//! (a pipe to the parent). There fd 2 is non-blocking while the sink
//! lives and blocking again after it, a second sink is refused
//! (`AlreadyExists`) and the claim is free again once the first is
//! dropped; the parent reads the line the sink wrote.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::io::{self, ErrorKind};
use std::os::fd::AsFd;
use std::process::{Command, Stdio};

use durchlauferhitzer::sys::fd::{Blocking, status_flags};
use durchlauferhitzer::{DriverLog, LogEntry, LogSink};

const CHILD: &str = "DURCHLAUFERHITZER_SINK_CHILD";

fn stderr_mode() -> Blocking {
    status_flags(io::stderr().as_fd()).unwrap().mode()
}

#[test]
fn the_stderr_sink_in_a_subprocess() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stderr_sink_child", "--test-threads=1"])
        .env(CHILD, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    // The child really ran the test (and was not filtered out).
    assert!(stdout.contains("1 passed"), "{stdout}");
    assert_eq!(stderr, "kind=dropped count=7\nkind=dropped count=8\n");
}

/// Runs only in the child (a no-op in the normal run).
#[test]
fn stderr_sink_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    assert_eq!(stderr_mode(), Blocking::Blocking);
    let mut sink = LogSink::stderr().unwrap();
    assert_eq!(stderr_mode(), Blocking::NonBlocking);
    let Err(err) = LogSink::stderr() else {
        panic!("a second stderr sink was created");
    };
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    // The refused sink did not touch the mode.
    assert_eq!(stderr_mode(), Blocking::NonBlocking);
    sink.write(LogEntry::Driver(DriverLog::Dropped { count: 7 }));
    drop(sink);
    assert_eq!(stderr_mode(), Blocking::Blocking);
    // The claim is free again.
    let mut again = LogSink::stderr().unwrap();
    again.write(LogEntry::Driver(DriverLog::Dropped { count: 8 }));
    drop(again);
    assert_eq!(stderr_mode(), Blocking::Blocking);
}
