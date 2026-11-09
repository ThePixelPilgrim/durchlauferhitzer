//! Range requests end to end (FR-STATIC-19): the 206 with exactly the
//! range's bytes (also across chunks, from a slow disk to a slow client),
//! the 416, `If-Range`, and the whole file for what is not served as a
//! range. Heads come from the spec-written model, bytes from the file.

use durchlauferhitzer_sim::Window;

use super::*;

/// A 600 KiB file whose bytes tell their positions apart.
fn pattern() -> Vec<u8> {
    (0..600 * 1024u32).map(|i| (i % 251) as u8).collect()
}

fn get_with(path: &str, lines: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: example.test\r\n{lines}\r\n").into_bytes()
}

fn partial(stamp: FileStamp, first: u64, last: u64) -> Vec<u8> {
    model_http::model_partial_head(
        "application/octet-stream",
        stamp,
        (first, last),
        Persistence::KeepAlive,
        None,
    )
}

/// `big.bin` (the pattern) in a fresh sim: the sim and the file's bytes
/// and stamp.
fn big_file() -> (Sim, Vec<u8>, FileStamp) {
    let mut sim = Sim::new(CONFIG);
    let bytes = pattern();
    sim.add_file("/srv/www", "big.bin", &bytes);
    let stamp = sim_stamp(bytes.len() as u64, 1);
    (sim, bytes, stamp)
}

/// Every readahead of the trace, as (offset, len).
fn readaheads(trace: &str) -> Vec<(u64, u64)> {
    trace
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" -> Readahead offset=")?;
            let (offset, rest) = rest.split_once(' ')?;
            let len = rest.strip_prefix("len=")?.split_whitespace().next()?;
            Some((offset.parse().ok()?, len.parse().ok()?))
        })
        .collect()
}

/// A range across a chunk boundary: exactly its bytes, read from its
/// first byte chunk by chunk, the connection kept for the next request.
#[test]
fn range_across_chunks_gets_exactly_its_bytes() {
    let (mut sim, bytes, stamp) = big_file();
    let c = sim.connect(peer());
    sim.send(c, &get_with("/big.bin", "Range: bytes=200000-500000\r\n"));
    sim.run_until_idle();
    let mut expected = partial(stamp, 200_000, 500_000);
    expected.extend_from_slice(&bytes[200_000..=500_000]);
    let received = sim.take_received(c);
    assert!(received == expected, "{}", sim.trace());
    assert_eq!(
        readaheads(sim.trace()),
        [(200_000, 262_144), (462_144, 37_857)],
        "{}",
        sim.trace()
    );
    assert_eq!(
        sim.logs(),
        [response_log(
            c,
            Status::PartialContent,
            Method::Get,
            "/big.bin"
        )]
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// A slow disk and a client that takes a few bytes at a time still get
/// exactly the range, and a suffix range the file's last bytes.
#[test]
fn slow_disk_and_slow_client_get_exactly_the_range() {
    let (mut sim, bytes, stamp) = big_file();
    sim.set_disk_delay(Duration::from_millis(50));
    let c = sim.connect(peer());
    sim.set_client_window(c, Window::Bytes(4096));
    sim.send(c, &get_with("/big.bin", "Range: bytes=262000-262300\r\n"));
    sim.run_until_idle();
    sim.advance(Duration::from_secs(1));
    let mut expected = partial(stamp, 262_000, 262_300);
    expected.extend_from_slice(&bytes[262_000..=262_300]);
    assert!(sim.take_received(c) == expected, "{}", sim.trace());
    sim.send(c, &get_with("/big.bin", "Range: bytes=-300000\r\n"));
    sim.run_until_idle();
    sim.advance(Duration::from_secs(5));
    let size = bytes.len() as u64;
    let mut expected = partial(stamp, size - 300_000, size - 1);
    expected.extend_from_slice(&bytes[bytes.len() - 300_000..]);
    assert!(sim.take_received(c) == expected, "{}", sim.trace());
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// A range past the end gets the 416 at once, nothing read, the
/// connection kept.
#[test]
fn range_past_the_end_gets_416() {
    let (mut sim, bytes, _) = big_file();
    let c = sim.connect(peer());
    let size = bytes.len() as u64;
    sim.send(
        c,
        &get_with("/big.bin", &format!("Range: bytes={size}-\r\n")),
    );
    sim.run_until_idle();
    assert_eq!(
        String::from_utf8_lossy(&sim.take_received(c)),
        String::from_utf8_lossy(&model_http::model_range_not_satisfiable(
            size,
            Persistence::KeepAlive,
            None
        ))
    );
    assert!(readaheads(sim.trace()).is_empty(), "{}", sim.trace());
    assert_eq!(
        sim.logs(),
        [response_log(
            c,
            Status::RangeNotSatisfiable,
            Method::Get,
            "/big.bin"
        )]
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// Owner decision 2026-11-09 (FR-STATIC-19): an empty file's `Range` is
/// ignored, a suffix range too: the whole (empty) file with `200`, the
/// connection kept.
#[test]
fn range_on_an_empty_file_gets_the_whole_200() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "empty.txt", b"");
    let stamp = sim_stamp(0, 1);
    let c = sim.connect(peer());
    for range in ["bytes=-5", "bytes=0-", "bytes=-0"] {
        sim.send(c, &get_with("/empty.txt", &format!("Range: {range}\r\n")));
        sim.run_until_idle();
        assert_eq!(
            String::from_utf8_lossy(&sim.take_received(c)),
            String::from_utf8_lossy(&head(
                "text/plain; charset=utf-8",
                stamp,
                Persistence::KeepAlive
            )),
            "{range}"
        );
    }
    assert_eq!(
        sim.logs(),
        vec![response_log(c, Status::Ok, Method::Get, "/empty.txt"); 3]
    );
    assert_eq!(sim.state(c), SimConnState::Open, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// `If-Range` with the file's `ETag` or exactly its `Last-Modified` keeps
/// the range; a stale or weak tag, or another date form, sends the whole
/// file with a 200.
#[test]
fn if_range_keeps_the_range_only_for_the_same_file() {
    let (mut sim, bytes, stamp) = big_file();
    let etag = model_http::model_etag(stamp);
    let last_modified = "Thu, 01 Jan 1970 00:00:00 GMT";
    let stale = model_http::model_etag(sim_stamp(stamp.size, 2));
    for (if_range, ranged) in [
        (etag.clone(), true),
        (last_modified.to_owned(), true),
        (stale, false),
        (format!("W/{etag}"), false),
        ("Thursday, 01-Jan-70 00:00:00 GMT".to_owned(), false),
    ] {
        let c = sim.connect(peer());
        let lines = format!("Range: bytes=10-19\r\nIf-Range: {if_range}\r\n");
        sim.send(c, &get_with("/big.bin", &lines));
        sim.run_until_idle();
        let expected = if ranged {
            let mut out = partial(stamp, 10, 19);
            out.extend_from_slice(&bytes[10..20]);
            out
        } else {
            let mut out = head("application/octet-stream", stamp, Persistence::KeepAlive);
            out.extend_from_slice(&bytes);
            out
        };
        assert!(
            sim.take_received(c) == expected,
            "{if_range}\n{}",
            sim.trace()
        );
    }
}

/// What is not served as a range gets the whole file: several ranges, a
/// malformed one, another unit, two `Range` lines; and a `HEAD` gets the
/// whole file's head.
#[test]
fn unserved_ranges_get_the_whole_file() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv/www", "a.txt", b"0123456789");
    let stamp = sim_stamp(10, 1);
    for lines in [
        "Range: bytes=0-1,5-6\r\n",
        "Range: bytes=5-1\r\n",
        "Range: items=0-1\r\n",
        "Range: bytes=0-0\r\nRange: bytes=1-1\r\n",
    ] {
        let c = sim.connect(peer());
        sim.send(c, &get_with("/a.txt", lines));
        sim.run_until_idle();
        let mut expected = head("text/plain; charset=utf-8", stamp, Persistence::KeepAlive);
        expected.extend_from_slice(b"0123456789");
        assert_eq!(sim.take_received(c), expected, "{lines}\n{}", sim.trace());
    }
    let c = sim.connect(peer());
    sim.send(
        c,
        b"HEAD /a.txt HTTP/1.1\r\nHost: example.test\r\nRange: bytes=0-1\r\n\r\n",
    );
    sim.run_until_idle();
    assert_eq!(
        sim.take_received(c),
        head("text/plain; charset=utf-8", stamp, Persistence::KeepAlive)
    );
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}

/// A file that shrinks below the range's end while it is sent is cut
/// like a whole file (FR-STATIC-15): the bytes before the shrink, then
/// the connection closes.
#[test]
fn a_range_cut_by_a_shrinking_file_closes() {
    let (mut sim, bytes, stamp) = big_file();
    sim.shrink_on_send("/srv/www", "big.bin", 1000);
    let c = sim.connect(peer());
    sim.send(c, &get_with("/big.bin", "Range: bytes=500-1999\r\n"));
    sim.run_until_idle();
    let mut expected = partial(stamp, 500, 1999);
    expected.extend_from_slice(&bytes[500..1000]);
    assert!(sim.take_received(c) == expected, "{}", sim.trace());
    assert!(
        sim.logs().contains(&Log::FileAborted {
            conn: c,
            peer: peer(),
            error: FileError::Truncated
        }),
        "{:?}",
        sim.logs()
    );
    assert_eq!(sim.state(c), SimConnState::Closed, "{}", sim.trace());
    assert_eq!(sim.open_files(), 0, "{}", sim.trace());
}
