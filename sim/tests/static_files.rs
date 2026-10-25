//! Static files through the simulation driver: the file interface between
//! core and driver, with the simulated file system behind it.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::{
    BodyMode, render_file_head, render_fixed, render_redirect,
};
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::http::target::TargetError;
use durchlauferhitzer_core::{
    ConnId, FileError, FileStamp, Interest, Log, RejectReason, RequestSummary, TimeoutKind,
};
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
/// is closed and logged as a disk timeout. A late `FileOpened` gets its
/// file closed; a readahead still in flight is superseded by `CloseFile`.
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
            reason: RejectReason::Timeout(TimeoutKind::Disk),
        }],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 0, "{}", sim.trace());
    sim.advance(Duration::from_secs(5));
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());

    // The second chunk's readahead is slower than the timeout.
    let mut sim = Sim::new(CONFIG);
    let bytes = vec![b'r'; 300 * 1024];
    sim.add_file("/srv/www", "r.bin", &bytes);
    let c = sim.connect(p);
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Stalled);
    sim.send(c, &get("/r.bin"));
    sim.run_until_idle();
    sim.set_disk_delay(Duration::from_secs(40));
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Unlimited);
    sim.run_until_idle();
    let head_len = head(
        "application/octet-stream",
        300 * 1024,
        Persistence::KeepAlive,
    )
    .len();
    assert_eq!(
        sim.take_received(c).len(),
        head_len + 256 * 1024,
        "{}",
        sim.trace()
    );
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: c,
            peer: p,
            reason: RejectReason::Timeout(TimeoutKind::Disk),
        }),
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    sim.advance(Duration::from_secs(15));
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(
        sim.trace().matches("ChunkReady").count(),
        1,
        "{}",
        sim.trace()
    );
}

/// Review Focus 2: one connection waits on a slow disk while another has a
/// stalled client; a third is served meanwhile.
#[test]
fn slow_disk_does_not_block_other_connections() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "slow.txt", b"slow");
    sim.add_file("/srv/www", "small.txt", b"small");
    sim.add_file("/srv/www", "big.bin", &vec![b'b'; 100_000]);
    // A stalled client with a ready chunk.
    let stalled = sim.connect(peer());
    sim.set_client_window(stalled, durchlauferhitzer_sim::Window::Stalled);
    sim.send(stalled, &get("/big.bin"));
    sim.run_until_idle();
    // A request stuck on the disk for 10 s.
    sim.set_disk_delay(Duration::from_secs(10));
    let a = sim.connect(peer());
    sim.send(a, &get("/slow.txt"));
    sim.run_until_idle();
    assert_eq!(sim.interest(a), Interest::None, "{}", sim.trace());
    // Meanwhile another connection gets a 404 and a small file.
    sim.set_disk_delay(Duration::ZERO);
    let b = sim.connect(peer());
    let mut both = get("/missing.txt");
    both.extend_from_slice(&get("/small.txt"));
    sim.send(b, &both);
    sim.run_until_idle();
    let mut expected = fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full);
    expected.extend_from_slice(&head(
        "text/plain; charset=utf-8",
        5,
        Persistence::KeepAlive,
    ));
    expected.extend_from_slice(b"small");
    assert_eq!(sim.take_received(b), expected, "{}", sim.trace());
    assert!(sim.take_received(a).is_empty(), "{}", sim.trace());
    assert!(sim.take_received(stalled).is_empty(), "{}", sim.trace());
    // The slow disk answers; the slow request is served.
    sim.advance(Duration::from_secs(10));
    let mut expected = head("text/plain; charset=utf-8", 4, Persistence::KeepAlive);
    expected.extend_from_slice(b"slow");
    assert_eq!(sim.take_received(a), expected, "{}", sim.trace());
    assert_eq!(sim.state(a), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.state(stalled), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 1, "{}", sim.trace());
}

/// A client that stops reading while its file is open: the send timeout
/// closes the connection and the file, once.
#[test]
fn stalled_client_with_open_file_times_out_and_closes_file() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "big.bin", &vec![b'b'; 100_000]);
    let p = peer();
    let c = sim.connect(p);
    let head_len = head("application/octet-stream", 100_000, Persistence::KeepAlive).len();
    sim.set_client_window(c, durchlauferhitzer_sim::Window::Budget(head_len + 10));
    sim.send(c, &get("/big.bin"));
    sim.run_until_idle();
    assert_eq!(sim.open_files(), 1, "{}", sim.trace());
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs().last(),
        Some(&Log::Rejected {
            conn: c,
            peer: p,
            reason: RejectReason::Timeout(TimeoutKind::Send),
        }),
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
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

/// Review Focus 1: `sendfile` hits end of file early. The client gets the
/// bytes that were there and then a cut connection, never a short body
/// that looks complete; the abort is logged and the file closed.
#[test]
fn file_shrinking_mid_transfer_cuts_connection() {
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
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Status::Ok, Method::Get, "/s.bin"),
            aborted(c, FileError::Truncated),
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

fn aborted(conn: ConnId, error: FileError) -> Log {
    Log::FileAborted {
        conn,
        peer: peer(),
        error,
    }
}

/// A 600 KiB file whose stamp changes to `new` after its first chunk was
/// read, served under `config`. Returns the connection and what it got.
fn changing_file(config: &str, new: FileStamp) -> (Sim, ConnId, Vec<u8>, Vec<u8>) {
    let mut sim = Sim::new(config);
    let bytes: Vec<u8> = (0..600 * 1024).map(|i: u32| (i % 241) as u8).collect();
    sim.add_file("/srv/www", "c.bin", &bytes);
    sim.change_during_send("/srv/www", "c.bin", 1, new);
    let (c, received) = exchange(&mut sim, &get("/c.bin"));
    let mut full = head(
        "application/octet-stream",
        bytes.len() as u64,
        Persistence::KeepAlive,
    );
    full.extend_from_slice(&bytes);
    (sim, c, received, full)
}

/// The first chunk is sent, then the second `ChunkReady` reports the change
/// and the connection is cut: the client got exactly the first chunk.
fn assert_aborted_after_first_chunk(
    sim: &Sim,
    c: ConnId,
    received: &[u8],
    full: &[u8],
    error: FileError,
) {
    let head_len = full.len() - 600 * 1024;
    assert!(
        full.get(..head_len + 256 * 1024) == Some(received),
        "got {} bytes\n{}",
        received.len(),
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Status::Ok, Method::Get, "/c.bin"),
            aborted(c, error),
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

const ALLOW_GROWTH: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; on_file_change allow_growth; }
}";

/// By default any change since open, even growth, cuts the connection.
#[test]
fn changed_file_aborts_by_default() {
    let new = FileStamp {
        size: 700 * 1024,
        modified_ns: 99,
    };
    let (sim, c, received, full) = changing_file(CONFIG, new);
    let ready: Vec<&str> = sim
        .trace()
        .lines()
        .filter(|l| l.contains(" ChunkReady "))
        .collect();
    assert_eq!(ready.len(), 2, "{}", sim.trace());
    assert!(
        ready[1].ends_with("size=716800 modified_ns=99"),
        "{}",
        sim.trace()
    );
    assert_aborted_after_first_chunk(&sim, c, &received, &full, FileError::Changed);
}

/// `allow_growth`: a file that only got larger keeps being sent at the
/// size seen at open; the connection stays usable.
#[test]
fn allow_growth_keeps_sending_grown_file() {
    let new = FileStamp {
        size: 700 * 1024,
        modified_ns: 99,
    };
    let (sim, c, received, full) = changing_file(ALLOW_GROWTH, new);
    assert!(
        received == full,
        "got {} bytes\n{}",
        received.len(),
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::Ok, Method::Get, "/c.bin")],
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.trace().matches(" ChunkReady ").count(),
        3,
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// `allow_growth`: a rewrite at the same size still aborts.
#[test]
fn allow_growth_aborts_on_same_size_rewrite() {
    let new = FileStamp {
        size: 600 * 1024,
        modified_ns: 99,
    };
    let (sim, c, received, full) = changing_file(ALLOW_GROWTH, new);
    assert_aborted_after_first_chunk(&sim, c, &received, &full, FileError::Changed);
}

/// `allow_growth`: a shrink still aborts.
#[test]
fn allow_growth_aborts_on_shrink() {
    let new = FileStamp {
        size: 500 * 1024,
        modified_ns: 99,
    };
    let (sim, c, received, full) = changing_file(ALLOW_GROWTH, new);
    assert_aborted_after_first_chunk(&sim, c, &received, &full, FileError::Truncated);
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
fn reset_during_open_closes_file_later() {
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
fn reset_during_readahead_closes_file() {
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

// ---------------------------------------------------------------------------
// Static route rules

fn fixed(status: Status, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    let mut out = Vec::new();
    render_fixed(status, persistence, body, &mut out);
    out
}

fn redirect(location: &str, persistence: Persistence, body: BodyMode) -> Vec<u8> {
    let mut out = Vec::new();
    render_redirect(location, persistence, body, &mut out);
    out
}

fn head_req(path: &str) -> Vec<u8> {
    format!("HEAD {path} HTTP/1.1\r\nHost: example.test\r\n\r\n").into_bytes()
}

fn response_log(conn: ConnId, status: Status, method: Method, path: &str) -> Log {
    Log::Response {
        conn,
        peer: peer(),
        status,
        request: Some(RequestSummary {
            method,
            host: Some("example.test".to_owned()),
            path: path.to_owned(),
        }),
    }
}

/// Send `request` on a fresh connection and run until idle.
fn exchange(sim: &mut Sim, request: &[u8]) -> (ConnId, Vec<u8>) {
    let c = sim.connect(peer());
    sim.send(c, request);
    sim.run_until_idle();
    let received = sim.take_received(c);
    (c, received)
}

/// A plain `GET` of `path` gets a keep-alive 404, logged once with the raw
/// path, and leaves no file open.
fn assert_404(sim: &mut Sim, path: &str) {
    let (c, received) = exchange(sim, &get(path));
    assert_eq!(
        received,
        fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::Full),
        "{path}\n{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{path}\n{}", sim.trace());
    let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
    assert_eq!(
        logs,
        [&response_log(c, Status::NotFound, Method::Get, path)],
        "{path}\n{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{path}\n{}", sim.trace());
}

fn log_conn(log: &Log) -> ConnId {
    match log {
        Log::Response { conn, .. }
        | Log::Rejected { conn, .. }
        | Log::Reset { conn, .. }
        | Log::FileAborted { conn, .. } => *conn,
    }
}

fn opens(trace: &str) -> usize {
    trace.lines().filter(|l| l.contains("-> OpenFile")).count()
}

#[test]
fn post_gets_405_with_allow() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    let (c, received) = exchange(
        &mut sim,
        b"POST /index.html HTTP/1.1\r\nHost: example.test\r\n\r\n",
    );
    let expected = fixed(
        Status::MethodNotAllowed,
        Persistence::KeepAlive,
        BodyMode::Full,
    );
    assert_eq!(received, expected, "{}", sim.trace());
    let text = String::from_utf8(received).unwrap();
    assert!(text.contains("\r\nAllow: GET, HEAD\r\n"), "{text}");
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::MethodNotAllowed,
            Method::Other("POST".to_owned()),
            "/index.html"
        )],
        "{}",
        sim.trace()
    );
}

#[test]
fn body_gets_413_and_closes() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "index.html", b"hello");
    let (c, received) = exchange(
        &mut sim,
        b"GET /index.html HTTP/1.1\r\nHost: example.test\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert_eq!(
        received,
        fixed(Status::ContentTooLarge, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::ContentTooLarge,
            Method::Get,
            "/index.html"
        )],
        "{}",
        sim.trace()
    );
    // Chunked bodies too.
    let (c, received) = exchange(
        &mut sim,
        b"GET /index.html HTTP/1.1\r\nHost: example.test\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    );
    assert_eq!(
        received,
        fixed(Status::ContentTooLarge, Persistence::Close, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}

#[test]
fn missing_file_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "a/b.txt", b"x");
    assert_404(&mut sim, "/nope.txt");
    assert_404(&mut sim, "/a/nope.txt");
    // A path through a regular file.
    assert_404(&mut sim, "/a/b.txt/c");
    assert_404(&mut sim, "/assets/nope.css");
}

#[test]
fn regular_file_with_trailing_slash_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/assets", "a.css", b"body{}");
    assert_404(&mut sim, "/assets/a.css/");
    assert!(sim.trace().contains("-> CloseFile"), "{}", sim.trace());
}

#[test]
fn symlink_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_symlink("/srv/www", "passwd", "/etc/passwd");
    sim.add_file("/srv/www", "real/x.txt", b"x");
    sim.add_file("/srv/www", "ln/x.txt", b"x");
    sim.add_symlink("/srv/www", "ln", "real");
    assert_404(&mut sim, "/passwd");
    assert_404(&mut sim, "/ln/x.txt");
}

#[test]
fn permission_denied_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "secret.txt", b"x");
    sim.add_file("/srv/www", "private/x.txt", b"x");
    sim.deny("/srv/www", "secret.txt");
    sim.deny("/srv/www", "private");
    assert_404(&mut sim, "/secret.txt");
    assert_404(&mut sim, "/private/x.txt");
}

#[test]
fn dotfile_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", ".env", b"SECRET=1");
    sim.add_file("/srv/www", "a/.git/config", b"[core]");
    assert_404(&mut sim, "/.env");
    assert_404(&mut sim, "/a/.git/config");
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}

#[test]
fn fifo_or_device_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_special("/srv/www", "fifo");
    sim.add_special("/srv/www", "dev/null");
    assert_404(&mut sim, "/fifo");
    assert_404(&mut sim, "/dev/null");
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
}

#[test]
fn directory_without_slash_redirects() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "docs/index.html", b"docs");
    let (c, received) = exchange(&mut sim, &get("/docs?x=1"));
    assert_eq!(
        received,
        b"HTTP/1.1 301 Moved Permanently\r\nLocation: /docs/?x=1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 22\r\nX-Content-Type-Options: nosniff\r\n\r\n301 Moved Permanently\n",
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::MovedPermanently,
            Method::Get,
            "/docs"
        )],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(opens(sim.trace()), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    // The root of a route without its slash; no query, no `?`.
    let (_, received) = exchange(&mut sim, &get("/assets"));
    assert_eq!(
        received,
        redirect("/assets/", Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
    // An empty query is kept as such.
    let (_, received) = exchange(&mut sim, &get("/docs?"));
    assert_eq!(
        received,
        redirect("/docs/?", Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn directory_with_slash_serves_index() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "docs/index.html", b"docs!");
    sim.add_file("/srv/www", "index.html", b"home");
    let (c, received) = exchange(&mut sim, &get("/docs/"));
    let mut expected = head("text/html; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"docs!");
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::Ok, Method::Get, "/docs/")],
        "{}",
        sim.trace()
    );
    assert!(
        sim.trace()
            .contains("file=2 -> OpenFile root=0 path=[\"docs\", \"index.html\"]"),
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    // The directory is closed before its index is opened.
    let close1 = sim.trace().find("file=1 -> CloseFile").unwrap();
    let open2 = sim.trace().find("file=2 -> OpenFile").unwrap();
    assert!(close1 < open2, "{}", sim.trace());
    // The root itself.
    let (_, received) = exchange(&mut sim, &get("/"));
    let mut expected = head("text/html; charset=utf-8", 4, Persistence::KeepAlive);
    expected.extend_from_slice(b"home");
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn directory_without_index_404() {
    let mut sim = Sim::new(CONFIG);
    sim.add_dir("/srv/www", "empty");
    sim.add_dir("/srv/www", "odd/index.html");
    sim.add_special("/srv/www", "weird/index.html");
    sim.add_file("/srv/www", "denied/index.html", b"x");
    sim.deny("/srv/www", "denied/index.html");
    assert_404(&mut sim, "/empty/");
    assert_404(&mut sim, "/odd/");
    assert_404(&mut sim, "/weird/");
    assert_404(&mut sim, "/denied/");
    assert_404(&mut sim, "/assets/");
}

/// Review Focus 5: HEAD on a large file sends the head with the full
/// `Content-Length`, reads nothing and closes the file.
#[test]
fn head_on_large_file_sends_head_only() {
    let mut sim = Sim::new(CONFIG);
    let size = 5 * 1024 * 1024;
    sim.add_file("/srv/www", "big.bin", &vec![b'b'; size]);
    let (c, received) = exchange(&mut sim, &head_req("/big.bin"));
    assert_eq!(
        received,
        head(
            "application/octet-stream",
            size as u64,
            Persistence::KeepAlive
        ),
        "{}",
        sim.trace()
    );
    assert!(!sim.trace().contains("Readahead"), "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.interest(c), Interest::Read, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::Ok, Method::Head, "/big.bin")],
        "{}",
        sim.trace()
    );
    // A pipelined GET after the HEAD gets the body.
    sim.add_file("/srv/www", "s.txt", b"small");
    let mut both = head_req("/s.txt");
    both.extend_from_slice(&get("/s.txt"));
    let (_, received) = exchange(&mut sim, &both);
    let mut expected = head("text/plain; charset=utf-8", 5, Persistence::KeepAlive);
    expected.extend_from_slice(&head(
        "text/plain; charset=utf-8",
        5,
        Persistence::KeepAlive,
    ));
    expected.extend_from_slice(b"small");
    assert_eq!(received, expected, "{}", sim.trace());
    // HEAD of a directory index.
    sim.add_file("/srv/www", "docs/index.html", b"docs!");
    let (_, received) = exchange(&mut sim, &head_req("/docs/"));
    assert_eq!(
        received,
        head("text/html; charset=utf-8", 5, Persistence::KeepAlive),
        "{}",
        sim.trace()
    );
    // Only the pipelined GET read anything.
    assert_eq!(
        sim.trace().matches("-> Readahead").count(),
        1,
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

#[test]
fn head_error_response_has_no_body() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "docs/index.html", b"docs");
    // Unknown host.
    let c = sim.connect(peer());
    sim.send(c, b"HEAD /x HTTP/1.1\r\nHost: other.test\r\n\r\n");
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    // Missing file, dotfile, directory without index.
    for path in ["/missing", "/.env", "/docs/x/"] {
        let (_, received) = exchange(&mut sim, &head_req(path));
        assert_eq!(
            received,
            fixed(Status::NotFound, Persistence::KeepAlive, BodyMode::HeadOnly),
            "{path}\n{}",
            sim.trace()
        );
    }
    // Redirect.
    let (_, received) = exchange(&mut sim, &head_req("/docs"));
    assert_eq!(
        received,
        redirect("/docs/", Persistence::KeepAlive, BodyMode::HeadOnly),
        "{}",
        sim.trace()
    );
    // A body.
    let (c, received) = exchange(
        &mut sim,
        b"HEAD /docs/ HTTP/1.1\r\nHost: example.test\r\nContent-Length: 2\r\n\r\nhi",
    );
    assert_eq!(
        received,
        fixed(
            Status::ContentTooLarge,
            Persistence::Close,
            BodyMode::HeadOnly
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

const WELL_KNOWN: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /.well-known { static /srv/well-known; }
}";

#[test]
fn well_known_route_serves_files() {
    let mut sim = Sim::new(WELL_KNOWN);
    sim.add_file("/srv/well-known", "acme-challenge/abc", b"token");
    sim.add_file("/srv/well-known", ".secret", b"no");
    let (_, received) = exchange(&mut sim, &get("/.well-known/acme-challenge/abc"));
    let mut expected = head("application/octet-stream", 5, Persistence::KeepAlive);
    expected.extend_from_slice(b"token");
    assert_eq!(received, expected, "{}", sim.trace());
    assert_404(&mut sim, "/.well-known/.secret");
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());

    // Without the route the prefix is just a dotfile under `/`.
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", ".well-known/x", b"x");
    assert_404(&mut sim, "/.well-known/x");
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}

// ---------------------------------------------------------------------------
// Deferred targeted tests

/// Two connection slots; a `docs/` directory with an index.
fn index_sim() -> Sim {
    let mut sim = Sim::new(CAPPED);
    sim.add_file("/srv/www", "docs/index.html", b"docs!");
    sim
}

/// The directory open (file 1) is answered after 10 s; the index open
/// (file 2) then takes `index_delay`. Returns the connection once the
/// index open is in flight.
fn index_open_in_flight(sim: &mut Sim, index_delay: Duration) -> ConnId {
    sim.set_disk_delay(Duration::from_secs(10));
    let c = sim.connect(peer());
    sim.send(c, &get("/docs/"));
    sim.run_until_idle();
    sim.set_disk_delay(index_delay);
    sim.advance(Duration::from_secs(10));
    assert!(
        sim.trace().contains("file=2 -> OpenFile"),
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 0, "{}", sim.trace());
    c
}

/// Whether a new connection is accepted (not refused with 503); it is
/// closed again either way.
fn slot_free(sim: &mut Sim) -> bool {
    let probe = sim.connect(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)));
    sim.run_until_idle();
    let refused = sim.take_received(probe).starts_with(b"HTTP/1.1 503 ");
    sim.reset(probe);
    sim.run_until_idle();
    !refused
}

/// A reset while the `index.html` open is in flight orphans that open: it
/// keeps its connection slot until the late `FileOpened`, which closes
/// the file exactly once.
#[test]
fn reset_during_index_open_orphans_it() {
    let mut sim = index_sim();
    let c = index_open_in_flight(&mut sim, Duration::from_secs(20));
    sim.reset(c);
    sim.run_until_idle();
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    // One slot is the orphan's, one the probe's: a second probe is refused.
    let holder = sim.connect(peer());
    sim.run_until_idle();
    assert!(!slot_free(&mut sim), "{}", sim.trace());
    sim.reset(holder);
    sim.advance(Duration::from_secs(20));
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    // The orphan's slot is free: a holder and a probe both fit.
    let _holder = sim.connect(peer());
    assert!(slot_free(&mut sim), "{}", sim.trace());
    sim.advance(Duration::from_secs(60));
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

/// `send_idle_timeout` passing while the `index.html` open is in flight is
/// a disk timeout; the orphan keeps its slot until the late `FileOpened`,
/// which closes the file exactly once.
#[test]
fn disk_timeout_during_index_open_orphans_it() {
    let mut sim = index_sim();
    let c = index_open_in_flight(&mut sim, Duration::from_secs(45));
    // The deadline was reset when the directory open completed (t=10 s).
    sim.advance(Duration::from_secs(30));
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[Log::Rejected {
            conn: c,
            peer: peer(),
            reason: RejectReason::Timeout(TimeoutKind::Disk),
        }],
        "{}",
        sim.trace()
    );
    let holder = sim.connect(peer());
    sim.run_until_idle();
    assert!(!slot_free(&mut sim), "{}", sim.trace());
    assert_eq!(close_files(sim.trace(), 2), 0, "{}", sim.trace());
    sim.reset(holder);
    // The index open was issued at t=10 s and completes at t=55 s.
    sim.advance(Duration::from_secs(15));
    assert_eq!(close_files(sim.trace(), 2), 1, "{}", sim.trace());
    let _holder = sim.connect(peer());
    assert!(slot_free(&mut sim), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
    assert!(sim.take_received(c).is_empty(), "{}", sim.trace());
}

const WITH_PROXY: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /api/ { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body unlimited; }
}";

/// Every response to a HEAD is head only, also the proxy route's 501. A
/// HEAD itself is never answered 405 (GET and HEAD are the allowed
/// methods), so the 405 keeps its body and a HEAD of the same path gets
/// the head of that path's answer.
#[test]
fn head_on_405_path_and_proxy_501_is_head_only() {
    let mut sim = Sim::new(WITH_PROXY);
    sim.add_file("/srv/www", "a.txt", b"hello");
    let (c, received) = exchange(&mut sim, &head_req("/api/x"));
    assert_eq!(
        received,
        fixed(
            Status::NotImplemented,
            Persistence::Close,
            BodyMode::HeadOnly
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::WriteShutdown, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(
            c,
            Status::NotImplemented,
            Method::Head,
            "/api/x"
        )],
        "{}",
        sim.trace()
    );
    // POST then HEAD of the same path on one connection.
    let mut both = b"POST /a.txt HTTP/1.1\r\nHost: example.test\r\n\r\n".to_vec();
    both.extend_from_slice(&head_req("/a.txt"));
    let (c, received) = exchange(&mut sim, &both);
    let mut expected = fixed(
        Status::MethodNotAllowed,
        Persistence::KeepAlive,
        BodyMode::Full,
    );
    expected.extend_from_slice(&head(
        "text/plain; charset=utf-8",
        5,
        Persistence::KeepAlive,
    ));
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(opens(sim.trace()), 1, "{}", sim.trace());
    assert!(!sim.trace().contains("Readahead"), "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// Under `abort`, a change already reported with the first chunk cuts the
/// connection before any body byte: the client sees at most the head.
#[test]
fn change_on_first_chunk_aborts_before_the_body() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "c.txt", b"hello");
    let new = FileStamp {
        size: 5,
        modified_ns: 99,
    };
    sim.change_during_send("/srv/www", "c.txt", 0, new);
    let (c, received) = exchange(&mut sim, &get("/c.txt"));
    let full_head = head("text/plain; charset=utf-8", 5, Persistence::KeepAlive);
    assert!(full_head.starts_with(&received), "{}", sim.trace());
    assert!(!sim.trace().contains("FileSent"), "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[
            response_log(c, Status::Ok, Method::Get, "/c.txt"),
            aborted(c, FileError::Changed),
        ],
        "{}",
        sim.trace()
    );
    assert_eq!(close_files(sim.trace(), 1), 1, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());

    // With a slow disk the head is out before the first chunk is read.
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "c.txt", b"hello");
    sim.change_during_send("/srv/www", "c.txt", 0, new);
    sim.set_disk_delay(Duration::from_millis(10));
    let c = sim.connect(peer());
    sim.send(c, &get("/c.txt"));
    sim.run_until_idle();
    sim.advance(Duration::from_secs(1));
    assert_eq!(sim.take_received(c), full_head, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.logs().last(), Some(&aborted(c, FileError::Changed)));
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

// ---------------------------------------------------------------------------
// Final-review fixes

/// Request lines long enough for a `301` that does not fit the 1 KiB send
/// buffer.
const LONG_LINES: &str = "listen 127.0.0.1:8080;
limits { max_request_line 16KiB; max_head 32KiB; }
host example.test { path / { static /srv/www; } }";

/// C1: a redirect whose `Location` would overflow the send buffer is
/// answered `414` instead, with the same persistence and body rules.
#[test]
fn redirect_too_large_for_the_send_buffer_is_414() {
    let mut sim = Sim::new(LONG_LINES);
    let dir = "a".repeat(600);
    sim.add_dir("/srv/www", &dir);
    let path = format!("/{dir}");
    let target = format!("{path}?{}", "q".repeat(3000));
    let (c, received) = exchange(&mut sim, &get(&target));
    assert_eq!(
        received,
        fixed(Status::UriTooLong, Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        &[response_log(c, Status::UriTooLong, Method::Get, &path)],
        "{}",
        sim.trace()
    );
    let (_, received) = exchange(&mut sim, &head_req(&target));
    assert_eq!(
        received,
        fixed(
            Status::UriTooLong,
            Persistence::KeepAlive,
            BodyMode::HeadOnly
        ),
        "{}",
        sim.trace()
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// C1 boundary: a redirect of exactly the send buffer's capacity is sent;
/// one byte more is a `414`.
#[test]
fn redirect_that_just_fits_is_301() {
    let mut sim = Sim::new(LONG_LINES);
    let dir = "a".repeat(600);
    sim.add_dir("/srv/www", &dir);
    let path = format!("/{dir}");
    let base = redirect(&format!("{path}/?"), Persistence::KeepAlive, BodyMode::Full).len();
    let fits = 1024 - base;
    let location = format!("{path}/?{}", "q".repeat(fits));
    let (c, received) = exchange(&mut sim, &get(&format!("{path}?{}", "q".repeat(fits))));
    let expected = redirect(&location, Persistence::KeepAlive, BodyMode::Full);
    assert_eq!(expected.len(), 1024);
    assert_eq!(received, expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    let (_, received) = exchange(&mut sim, &get(&format!("{path}?{}", "q".repeat(fits + 1))));
    assert_eq!(
        received,
        fixed(Status::UriTooLong, Persistence::KeepAlive, BodyMode::Full),
        "{}",
        sim.trace()
    );
}

/// C2: a percent-encoded unreserved character in the path is rejected, so
/// `/%61ssets/a.css` cannot slip past the `/assets` route into `/`.
#[test]
fn encoded_unreserved_character_in_the_path_is_400() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/assets", "a.css", b"body{}");
    sim.add_file("/srv/www", "assets/a.css", b"shadowed");
    for target in ["/%61ssets/a.css", "/assets/%61.css", "/assets/a.c%73s"] {
        let (c, received) = exchange(&mut sim, &get(target));
        assert_eq!(
            received,
            fixed(Status::BadRequest, Persistence::Close, BodyMode::Full),
            "{target}\n{}",
            sim.trace()
        );
        let logs: Vec<&Log> = sim.logs().iter().filter(|l| log_conn(l) == c).collect();
        assert_eq!(
            logs,
            [&Log::Rejected {
                conn: c,
                peer: peer(),
                reason: RejectReason::Http(HttpError::BadTarget(TargetError::EncodedUnreserved)),
            }],
            "{target}\n{}",
            sim.trace()
        );
    }
    assert_eq!(opens(sim.trace()), 0, "{}", sim.trace());
}
