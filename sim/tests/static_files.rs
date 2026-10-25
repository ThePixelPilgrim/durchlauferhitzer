//! Static files through the simulation driver: the file interface between
//! core and driver, with the simulated file system behind it.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use durchlauferhitzer_core::http::request::Persistence;
use durchlauferhitzer_core::http::response::render_file_head;
use durchlauferhitzer_core::{FileStamp, Interest, Log, RejectReason, TimeoutKind};
use durchlauferhitzer_sim::{Sim, SimConnState};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /assets { static /srv/assets; }
}";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

fn head(content_type: &str, len: u64, persistence: Persistence) -> Vec<u8> {
    let mut out = Vec::new();
    render_file_head(content_type, len, persistence, &mut out);
    out
}

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: example.test\r\n\r\n").into_bytes()
}

#[test]
fn serves_small_file() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    let c = sim.connect(peer());
    sim.send(c, &get("/index.html"));
    sim.run_until_idle();
    let mut expected = head("text/html; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"hello");
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// Sum of `FileSent` lengths seen in the trace before each `Readahead`,
/// paired with that readahead's offset.
fn readaheads_with_bytes_sent(trace: &str) -> Vec<(u64, u64)> {
    let mut sent = 0;
    let mut out = Vec::new();
    for line in trace.lines() {
        if let Some((_, rest)) = line.split_once(" FileSent len=") {
            sent += rest.parse::<u64>().unwrap_or(0);
        }
        if let Some((_, rest)) = line.split_once(" -> Readahead offset=") {
            let offset = rest
                .split_whitespace()
                .next()
                .and_then(|o| o.parse::<u64>().ok())
                .unwrap_or(u64::MAX);
            out.push((offset, sent));
        }
    }
    out
}

#[test]
fn serves_file_larger_than_one_chunk() {
    let mut sim = Sim::new(CONFIG);
    let bytes: Vec<u8> = (0..600 * 1024).map(|i: u32| (i % 251) as u8).collect();
    sim.add_file("/srv/www", "big.bin", &bytes);
    let c = sim.connect(peer());
    sim.send(c, &get("/big.bin"));
    sim.run_until_idle();
    let mut expected = head(
        "application/octet-stream",
        bytes.len() as u64,
        Persistence::KeepAlive,
    );
    expected.extend_from_slice(&bytes);
    let received = sim.take_received(c);
    assert!(received == expected, "{}", sim.trace());
    let readaheads = readaheads_with_bytes_sent(sim.trace());
    assert_eq!(readaheads.len(), 3, "{}", sim.trace());
    for (offset, sent) in readaheads {
        assert_eq!(offset, sent, "{}", sim.trace());
    }
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn alias_semantics_strip_route_prefix() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/assets", "a.css", b"body{}");
    let c = sim.connect(peer());
    sim.send(c, &get("/assets/a.css"));
    sim.run_until_idle();
    let mut expected = head("text/css; charset=utf-8", 6, Persistence::KeepAlive);
    expected.extend_from_slice(b"body{}");
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn slow_disk_still_delivers() {
    let mut sim = Sim::new(CONFIG);
    let bytes = vec![b'z'; 300 * 1024];
    sim.add_file("/srv/www", "slow.txt", &bytes);
    sim.set_disk_delay(Duration::from_millis(50));
    let c = sim.connect(peer());
    sim.send(c, &get("/slow.txt"));
    sim.run_until_idle();
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::None, "{}", sim.trace());
    sim.advance(Duration::from_secs(1));
    let mut expected = head(
        "text/plain; charset=utf-8",
        bytes.len() as u64,
        Persistence::KeepAlive,
    );
    expected.extend_from_slice(&bytes);
    assert!(sim.take_received(c) == expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn static_roots_are_listed() {
    let sim = Sim::new(CONFIG);
    assert_eq!(
        sim.static_roots(),
        [PathBuf::from("/srv/www"), PathBuf::from("/srv/assets")]
    );
}

/// Waiting on the disk counts against `send_idle_timeout`: the connection
/// is closed, and the file the late `FileOpened` reports is closed too.
#[test]
fn disk_slower_than_send_idle_timeout_closes() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    sim.set_disk_delay(Duration::from_secs(31));
    let p = peer();
    let c = sim.connect(p);
    sim.send(c, &get("/index.html"));
    sim.run_until_idle();
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: p,
            reason: RejectReason::Timeout(TimeoutKind::Send),
        }],
        "{}",
        sim.trace()
    );
    sim.advance(Duration::from_secs(5));
    assert!(sim.trace().contains("-> CloseFile"), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// The client reads slowly: chunks still follow one another and the file
/// is closed after the last byte.
#[test]
fn slow_client_gets_the_whole_file() {
    let mut sim = Sim::new(CONFIG);
    let bytes: Vec<u8> = (0..300 * 1024).map(|i: u32| (i % 13) as u8).collect();
    sim.add_file("/srv/www", "f.bin", &bytes);
    let c = sim.connect(peer());
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Bytes(7000));
    sim.send(
        c,
        b"GET /f.bin HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    sim.run_until_idle();
    let mut expected = head(
        "application/octet-stream",
        bytes.len() as u64,
        Persistence::Close,
    );
    expected.extend_from_slice(&bytes);
    assert!(sim.take_received(c) == expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// `sendfile` hits end of file early: the connection is cut after the bytes
/// that were there, and the file is closed. (Task 4 adds the policy.)
#[test]
fn file_shrinking_during_send_cuts_the_connection() {
    let mut sim = Sim::new(CONFIG);
    let bytes = vec![b'q'; 1000];
    sim.add_file("/srv/www", "s.bin", &bytes);
    sim.shrink_on_send("/srv/www", "s.bin", 400);
    let c = sim.connect(peer());
    sim.send(c, &get("/s.bin"));
    sim.run_until_idle();
    let mut expected = head("application/octet-stream", 1000, Persistence::KeepAlive);
    expected.extend_from_slice(&bytes[..400]);
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert!(
        sim.trace().contains("FileFailed error=Truncated"),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// Later chunks report the changed stamp; this task records it only.
#[test]
fn changed_stamp_is_reported_with_later_chunks() {
    let mut sim = Sim::new(CONFIG);
    let bytes = vec![b'c'; 600 * 1024];
    sim.add_file("/srv/www", "c.bin", &bytes);
    let new = FileStamp {
        size: 700 * 1024,
        modified_ns: 99,
    };
    sim.change_during_send("/srv/www", "c.bin", 1, new);
    let c = sim.connect(peer());
    sim.send(c, &get("/c.bin"));
    sim.run_until_idle();
    let ready: Vec<&str> = sim
        .trace()
        .lines()
        .filter(|l| l.contains(" ChunkReady "))
        .collect();
    assert_eq!(ready.len(), 3, "{}", sim.trace());
    assert!(!ready[0].contains("modified_ns=99"), "{}", sim.trace());
    assert!(
        ready[1].ends_with("size=716800 modified_ns=99"),
        "{}",
        sim.trace()
    );
    assert!(
        ready[2].ends_with("size=716800 modified_ns=99"),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

const CAPPED: &str = "listen 127.0.0.1:8080;
limits { max_connections 2; }
host example.test { path / { static /srv/www; } }";

/// Opens orphaned by a hung disk keep their connection slots, so they
/// cannot pile up beyond `max_connections`.
#[test]
fn orphaned_opens_hold_connection_slots() {
    let mut sim = Sim::new(CAPPED);
    sim.add_file("/srv/www", "index.html", b"hello");
    sim.set_disk_delay(Duration::from_secs(3600));
    for _ in 0..2 {
        let c = sim.connect(peer());
        sim.send(c, &get("/index.html"));
        sim.run_until_idle();
        sim.reset(c);
    }
    let refused = sim.connect(peer());
    sim.run_until_idle();
    let received = sim.take_received(refused);
    assert!(received.starts_with(b"HTTP/1.1 503 "), "{}", sim.trace());
    sim.advance(Duration::from_secs(3600));
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    sim.set_disk_delay(Duration::ZERO);
    let c = sim.connect(peer());
    sim.send(c, &get("/index.html"));
    sim.run_until_idle();
    let mut expected = head("text/html; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"hello");
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
}

/// How often the trace closes `file`.
fn close_files(trace: &str, file: u64) -> usize {
    let needle = format!("file={file} -> CloseFile");
    trace.lines().filter(|l| l.ends_with(&needle)).count()
}

/// The driver sends from the file it opened: replacing the path afterwards
/// does not change the bytes sent.
#[test]
fn sendfile_reads_the_file_as_opened() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "f.txt", b"hello");
    let c = sim.connect(peer());
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Stalled);
    sim.send(c, &get("/f.txt"));
    sim.run_until_idle();
    sim.add_file("/srv/www", "f.txt", b"WORLD");
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Unlimited);
    sim.run_until_idle();
    let mut expected = head("text/plain; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"hello");
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
}

/// A connection reset while its file is being opened: the late
/// `FileOpened` closes the file, exactly once.
#[test]
fn reset_during_open_closes_the_file_once() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    sim.set_disk_delay(Duration::from_millis(50));
    let c = sim.connect(peer());
    sim.send(c, &get("/index.html"));
    sim.run_until_idle();
    sim.reset(c);
    assert_eq!(close_files(sim.trace(), 1), 0, "{}", sim.trace());
    sim.advance(Duration::from_secs(1));
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert!(!sim.trace().contains("Readahead"), "{}", sim.trace());
}

/// A connection reset while a readahead is in flight: the file is closed at
/// once and the readahead's completion is never delivered.
#[test]
fn reset_during_readahead_closes_the_file_once() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    sim.set_disk_delay(Duration::from_millis(50));
    let c = sim.connect(peer());
    sim.send(c, &get("/index.html"));
    sim.run_until_idle();
    sim.advance(Duration::from_millis(50));
    assert!(sim.trace().contains("-> Readahead"), "{}", sim.trace());
    assert!(!sim.trace().contains("ChunkReady"), "{}", sim.trace());
    sim.reset(c);
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    sim.advance(Duration::from_secs(1));
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert!(!sim.trace().contains("ChunkReady"), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// A connection reset while a segment is ready but the client is not
/// reading: the file is closed exactly once.
#[test]
fn reset_with_a_ready_segment_closes_the_file_once() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "big.bin", &vec![b'b'; 100_000]);
    let c = sim.connect(peer());
    let head_len = head("application/octet-stream", 100_000, Persistence::KeepAlive).len();
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Budget(head_len + 10));
    sim.send(c, &get("/big.bin"));
    sim.run_until_idle();
    assert_eq!(sim.take_received(c).len(), head_len + 10, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Write, "{}", sim.trace());
    sim.reset(c);
    sim.run_until_idle();
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// A second GET pipelined behind the first is answered after the first
/// file response, from a second file.
#[test]
fn pipelined_get_after_a_file_response() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "a.txt", b"first");
    sim.add_file("/srv/assets", "b.css", b"second!");
    let c = sim.connect(peer());
    let mut both = get("/a.txt");
    both.extend_from_slice(&get("/assets/b.css"));
    sim.send(c, &both);
    sim.run_until_idle();
    let mut expected = head("text/plain; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"first");
    expected.extend_from_slice(&head("text/css; charset=utf-8", 7, Persistence::KeepAlive));
    expected.extend_from_slice(b"second!");
    assert_eq!(sim.take_received(c), expected, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}
