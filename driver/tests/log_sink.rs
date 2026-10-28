//! The non-blocking log sink (FR-LOG-06) on a pipe, never the real
//! stderr: `O_NONBLOCK` while the sink lives and the original flags back
//! after it; a reader that stalls never blocks a write, whole entries are
//! dropped and counted, and the `dropped` line comes once, when there is
//! room again, before newer entries; no line is ever cut.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

use std::net::{IpAddr, Ipv4Addr};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use durchlauferhitzer::log::{Buffered, format_driver, format_entry};
use durchlauferhitzer::sys::fd::{Blocking, Io, pipe, set_status_flags, status_flags};
use durchlauferhitzer::sys::socket::read;
use durchlauferhitzer::{DriverLog, LogEntry, LogSink};
use durchlauferhitzer_core::{ConnId, Log};

const PATIENCE: Duration = Duration::from_secs(5);

/// The sink's buffer (FR-LOG-06).
const BUFFER: usize = 64 * 1024;

fn reset(conn: u64) -> LogEntry {
    LogEntry::Core(Log::Reset {
        conn: ConnId(conn),
        peer: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)),
    })
}

fn line_of(entry: &LogEntry) -> Vec<u8> {
    let mut out = Vec::new();
    match entry {
        LogEntry::Core(log) => format_entry(log, &[], &mut out),
        LogEntry::Driver(log) => format_driver(log, &mut out),
    }
    out
}

/// Reads what the pipe holds now into `got`.
fn read_available(reader: &OwnedFd, got: &mut Vec<u8>) {
    let mut buf = [0u8; 8192];
    loop {
        match read(reader.as_fd(), &mut buf) {
            Io::Done(n) => got.extend_from_slice(&buf[..n]),
            Io::WouldBlock => return,
            other @ (Io::Closed | Io::Err(_)) => panic!("{other:?}"),
        }
    }
}

fn non_blocking_reader(reader: &OwnedFd) {
    let flags = status_flags(reader.as_fd()).unwrap();
    set_status_flags(reader.as_fd(), flags.with(Blocking::NonBlocking)).unwrap();
}

/// R3: the flags belong to the file description, which another process
/// (the shell behind a terminal) may share: the sink sets `O_NONBLOCK`
/// and puts the original flags back when it is dropped.
#[test]
fn the_sink_sets_non_blocking_and_restores_the_flags() {
    let (_reader, writer) = pipe().unwrap();
    let other = writer.try_clone().unwrap();
    let before = status_flags(other.as_fd()).unwrap();
    assert_eq!(before.mode(), Blocking::Blocking);
    let sink = LogSink::fd(writer).unwrap();
    assert_eq!(
        status_flags(other.as_fd()).unwrap().mode(),
        Blocking::NonBlocking
    );
    drop(sink);
    assert_eq!(status_flags(other.as_fd()).unwrap(), before);
}

/// Review m1: on drop the sink puts back only the blocking mode, so a
/// flag someone else changed while it lived (here `O_APPEND`, copied from
/// a descriptor opened for appending) stays as they set it.
#[test]
fn the_sink_restores_only_the_blocking_mode() {
    let (_reader, writer) = pipe().unwrap();
    let other = writer.try_clone().unwrap();
    let sink = LogSink::fd(writer).unwrap();
    let append = std::fs::OpenOptions::new()
        .append(true)
        .open("/dev/null")
        .unwrap();
    let appending = status_flags(append.as_fd()).unwrap();
    set_status_flags(other.as_fd(), appending.with(Blocking::NonBlocking)).unwrap();
    let changed = status_flags(other.as_fd()).unwrap();
    assert_eq!(changed.mode(), Blocking::NonBlocking);
    drop(sink);
    assert_eq!(
        status_flags(other.as_fd()).unwrap(),
        changed.with(Blocking::Blocking)
    );
}

/// A sink on a reader that keeps up writes each entry as one line, at
/// once, and has nothing buffered.
#[test]
fn entries_are_written_as_lines_at_once() {
    let (reader, writer) = pipe().unwrap();
    non_blocking_reader(&reader);
    let mut sink = LogSink::fd(writer).unwrap();
    let entries = [
        reset(1),
        LogEntry::Driver(DriverLog::AcceptFailed {
            errno: libc::EMFILE,
        }),
    ];
    for entry in &entries {
        sink.write(entry.clone());
    }
    assert_eq!(sink.buffered(), Buffered::Nothing);
    let mut got = Vec::new();
    read_available(&reader, &mut got);
    let expected: Vec<u8> = entries.iter().flat_map(line_of).collect();
    assert_eq!(
        String::from_utf8(got).unwrap(),
        String::from_utf8(expected).unwrap()
    );
}

/// FR-LOG-06 with a reader that stalls: the writes never block (they run
/// in a thread the test waits for), the pipe and then the 64 KiB buffer
/// fill up, later entries are dropped whole and counted; once the reader
/// drains the pipe and the sink is flushed (the loop does that on
/// `EPOLLOUT`), every kept entry arrives whole and in order, followed by
/// one `dropped` line with the exact count, and a newer entry after it.
#[test]
fn a_stalled_reader_drops_whole_entries_and_counts_them() {
    const ENTRIES: u64 = 40_000;
    let (reader, writer) = pipe().unwrap();
    non_blocking_reader(&reader);
    let mut sink = LogSink::fd(writer).unwrap();

    let (done, finished) = mpsc::channel();
    let burst = thread::spawn(move || {
        for conn in 1..=ENTRIES {
            sink.write(reset(conn));
        }
        done.send(()).unwrap();
        sink
    });
    finished
        .recv_timeout(PATIENCE)
        .expect("a write blocked on the stalled pipe");
    let mut sink = burst.join().unwrap();
    assert_eq!(sink.buffered(), Buffered::Bytes);

    let mut got = Vec::new();
    loop {
        read_available(&reader, &mut got);
        sink.flush();
        if sink.buffered() == Buffered::Nothing {
            read_available(&reader, &mut got);
            break;
        }
    }

    assert_eq!(got.last(), Some(&b'\n'));
    let text = String::from_utf8(got).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let kept = lines.len() - 1;
    for (i, line) in lines[..kept].iter().enumerate() {
        let expected = line_of(&reset(i as u64 + 1));
        assert_eq!(line.as_bytes(), &expected[..expected.len() - 1], "line {i}");
    }
    let dropped = ENTRIES - kept as u64;
    assert!(dropped > 0);
    assert_eq!(lines[kept], format!("kind=dropped count={dropped}"));
    // The buffer alone held a buffer's worth of whole entries.
    let line_len = line_of(&reset(ENTRIES)).len();
    assert!(
        kept * line_len >= BUFFER - line_len,
        "only {kept} entries kept"
    );

    // Output caught up: a newer entry comes alone, with no second
    // `dropped` line.
    sink.write(reset(ENTRIES + 1));
    assert_eq!(sink.buffered(), Buffered::Nothing);
    let mut more = Vec::new();
    read_available(&reader, &mut more);
    assert_eq!(more, line_of(&reset(ENTRIES + 1)));
}

/// A reader that went away (`EPIPE`; the test harness ignores SIGPIPE):
/// the entries are lost, but a write neither fails nor keeps bytes the
/// loop would wait to write forever (an `EPOLLOUT` registration on a
/// broken pipe would spin).
#[test]
fn a_reader_that_went_away_leaves_nothing_buffered() {
    let (reader, writer) = pipe().unwrap();
    drop(reader);
    let mut sink = LogSink::fd(writer).unwrap();
    for conn in 1..=3 {
        sink.write(reset(conn));
        assert_eq!(sink.buffered(), Buffered::Nothing);
    }
    sink.flush();
    assert_eq!(sink.buffered(), Buffered::Nothing);
}

/// The channel sink of the tests never buffers.
#[test]
fn the_channel_sink_buffers_nothing() {
    let (mut sink, entries) = LogSink::channel();
    sink.write(reset(1));
    assert_eq!(sink.buffered(), Buffered::Nothing);
    assert_eq!(entries.try_iter().collect::<Vec<_>>(), [reset(1)]);
}
