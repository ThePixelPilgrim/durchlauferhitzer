//! FR-LOG-06 in the running loop: a log that backed up while its reader
//! stalled is written by the loop itself once the reader catches up
//! (on `EPOLLOUT`, with no new entry to push it), the `dropped` line
//! follows the kept entries once, and the idle loop does not spin.
//!
//! It measures the process's CPU time, so this binary holds exactly this
//! one test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsFd, OwnedFd};
use std::thread;
use std::time::{Duration, Instant};

use durchlauferhitzer::log::{Buffered, format_entry};
use durchlauferhitzer::sys::fd::{Blocking, Io, pipe, set_status_flags, status_flags};
use durchlauferhitzer::sys::socket::read;
use durchlauferhitzer::{Driver, Exit, LogEntry, LogSink};
use durchlauferhitzer_core::config::load;
use durchlauferhitzer_core::{ConnId, Core, Log};

const PATIENCE: Duration = Duration::from_secs(5);

/// No static roots, so no disk pool; no listeners.
const CONFIG: &str = "listen 127.0.0.1:8080; host a.test { path / { proxy 127.0.0.1:9; \
    max_request_body 1MiB; max_response_body 1MiB; } }";

/// The CPU time the process used so far, in clock ticks (`USER_HZ`).
fn cpu_ticks(stat: &mut File) -> u64 {
    let mut line = String::new();
    stat.seek(SeekFrom::Start(0)).unwrap();
    stat.read_to_string(&mut line).unwrap();
    let rest = &line[line.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn reset(conn: u64) -> Log {
    Log::Reset {
        conn: ConnId(conn),
        peer: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)),
    }
}

fn line_of(log: &Log) -> String {
    let mut out = Vec::new();
    format_entry(log, &mut out);
    String::from_utf8(out).unwrap()
}

/// Reads what the pipe holds now; whether anything came.
fn read_available(reader: &OwnedFd, got: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 8192];
    let mut any = false;
    loop {
        match read(reader.as_fd(), &mut buf) {
            Io::Done(n) => {
                got.extend_from_slice(&buf[..n]);
                any = true;
            }
            Io::WouldBlock => return any,
            other @ (Io::Closed | Io::Err(_)) => panic!("{other:?}"),
        }
    }
}

#[test]
fn the_loop_writes_a_backed_up_log_once_the_reader_catches_up() {
    const ENTRIES: u64 = 40_000;
    let (reader, writer) = pipe().unwrap();
    let flags = status_flags(reader.as_fd()).unwrap();
    set_status_flags(reader.as_fd(), flags.with(Blocking::NonBlocking)).unwrap();

    // Back the log up before the loop runs: the pipe and the buffer are
    // full, later entries were dropped.
    let mut log = LogSink::fd(writer).unwrap();
    for conn in 1..=ENTRIES {
        log.write(LogEntry::Core(reset(conn)));
    }
    assert_eq!(log.buffered(), Buffered::Bytes);

    let core = Core::new(load(CONFIG).unwrap());
    let mut driver = Driver::new(core, Vec::new(), Vec::new(), 0, log).unwrap();
    let stop = driver.stop_handle();
    let thread = thread::spawn(move || driver.run());

    // Only the loop writes now. Read until the `dropped` line arrives.
    let start = Instant::now();
    let mut got = Vec::new();
    while !String::from_utf8_lossy(&got).contains("kind=dropped") {
        assert!(
            start.elapsed() < PATIENCE,
            "the loop did not write the log: {} bytes so far",
            got.len()
        );
        if !read_available(&reader, &mut got) {
            thread::sleep(Duration::from_millis(1));
        }
    }
    thread::sleep(Duration::from_millis(50));
    read_available(&reader, &mut got);

    let text = String::from_utf8(got).unwrap();
    assert!(text.ends_with('\n'));
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines.len() - 1;
    for (i, line) in lines[..kept].iter().enumerate() {
        assert_eq!(
            format!("{line}\n"),
            line_of(&reset(i as u64 + 1)),
            "line {i}"
        );
    }
    assert_eq!(
        lines[kept],
        format!("kind=dropped count={}", ENTRIES - kept as u64)
    );

    // Caught up and idle: the always-writable pipe does not wake the
    // loop.
    let mut stat = File::open("/proc/self/stat").unwrap();
    let before = cpu_ticks(&mut stat);
    thread::sleep(Duration::from_secs(1));
    let cpu = cpu_ticks(&mut stat) - before;
    assert!(cpu < 30, "{cpu} ticks of CPU in 1 s: the loop spins");

    stop.stop().unwrap();
    assert_eq!(thread.join().unwrap().unwrap(), Exit::Stopped);
    // The driver is gone and with it the write end: nothing more came.
    let mut buf = [0u8; 64];
    assert!(matches!(read(reader.as_fd(), &mut buf), Io::Closed));
}
