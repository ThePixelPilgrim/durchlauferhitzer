//! DRV-16 on a real kernel: an `accept` that runs out of descriptors
//! pauses the listeners, the loop does not spin, and the 1 s retry resumes
//! them once descriptors are free again, with no connection closing in
//! between.
//!
//! The descriptor limit and table are process-wide, so this binary holds
//! exactly this one test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use durchlauferhitzer::sys::rlimit::{nofile, set_soft_nofile};
use durchlauferhitzer::{DriverLog, LogEntry};

#[path = "support/server.rs"]
mod server;

use server::{PATIENCE, Server, backend, config, read_response, text};

/// The CPU time the process used so far, in clock ticks (`USER_HZ`, 100
/// per second on Linux), from a `/proc/self/stat` opened beforehand: no
/// descriptor is needed while they are exhausted.
fn cpu_ticks(stat: &mut File) -> u64 {
    let mut line = String::new();
    stat.seek(SeekFrom::Start(0)).unwrap();
    stat.read_to_string(&mut line).unwrap();
    // After the `(comm)` field: state is field 3, utime 14, stime 15.
    let rest = &line[line.rfind(')').unwrap() + 2..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields[11].parse().unwrap();
    let stime: u64 = fields[12].parse().unwrap();
    utime + stime
}

const EMFILE: LogEntry = LogEntry::Driver(DriverLog::AcceptFailed {
    errno: libc::EMFILE,
});

#[test]
fn out_of_descriptors_pauses_accepting_and_the_timer_resumes_it() {
    let (_backend, backend_addr) = backend();
    let server = Server::start(&config(backend_addr));
    let mut stat = File::open("/proc/self/stat").unwrap();
    let (soft, hard) = nofile().unwrap();

    // Leave room for a few descriptors, then take all of them but one.
    let open = u64::try_from(fs::read_dir("/proc/self/fd").unwrap().count()).unwrap();
    let limit = open + 16;
    set_soft_nofile(limit).unwrap();
    assert_eq!(nofile().unwrap(), (limit, hard));
    let mut fillers = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(file) => fillers.push(file),
            Err(err) if err.raw_os_error() == Some(libc::EMFILE) => break,
            Err(err) => panic!("{err}"),
        }
    }
    fillers.pop();

    // The client takes the last descriptor; the driver's accept fails.
    let cpu_before = cpu_ticks(&mut stat);
    let start = Instant::now();
    let mut client = TcpStream::connect(server.addr).unwrap();
    client.set_read_timeout(Some(PATIENCE)).unwrap();
    assert_eq!(server.logs.recv_timeout(PATIENCE).unwrap(), EMFILE);

    // For 1.5 s: one more retry, at 1 s, logged once; no spin.
    let window = Duration::from_millis(1500);
    let mut logs = Vec::new();
    while let Some(left) = window.checked_sub(start.elapsed()) {
        match server.logs.recv_timeout(left) {
            Ok(entry) => logs.push(entry),
            Err(_) => break,
        }
    }
    assert_eq!(logs, [EMFILE]);
    let cpu = cpu_ticks(&mut stat) - cpu_before;
    assert!(cpu < 30, "{cpu} ticks of CPU in 1.5 s: the loop spins");

    // Free the descriptors: the next retry (at 2 s) takes the client,
    // which no closed connection could have resumed.
    drop(fillers);
    set_soft_nofile(soft).unwrap();
    assert_eq!(nofile().unwrap(), (soft, hard));
    // Above the hard limit fails and changes nothing; an unlimited hard
    // limit has nothing above it.
    if hard != u64::MAX {
        assert_eq!(
            set_soft_nofile(hard + 1).map_err(|e| e.kind()),
            Err(ErrorKind::InvalidInput)
        );
        assert_eq!(nofile().unwrap(), (soft, hard));
    }
    client
        .write_all(b"GET /x HTTP/1.1\r\nHost: other.test\r\n\r\n")
        .unwrap();
    let response = read_response(&mut client);
    assert!(
        text(&response).starts_with("HTTP/1.1 404 "),
        "{}",
        text(&response)
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1900) && elapsed < Duration::from_millis(3000),
        "served after {elapsed:?}"
    );
    let (_, entries) = server.stop();
    assert!(
        entries.iter().all(|e| matches!(e, LogEntry::Core(_))),
        "{entries:?}"
    );
}
