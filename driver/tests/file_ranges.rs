//! Range requests through the built binary (FR-STATIC-19), under the
//! seccomp kill switch when it is on: a 600 KiB pattern file in a static
//! root on the real disk, sent with `sendfile` from a range's first byte
//! across a chunk boundary, a suffix range, and a 416. Each head is
//! compared byte for byte with the sim's spec-written model
//! (`sim/tests/support/model_http.rs`, with the `Date` the response
//! carried), each body with the file's bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

#[allow(dead_code, reason = "shared with the sim's test binaries; we use part")]
#[path = "../../sim/tests/support/model_http.rs"]
mod model_http;

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::UNIX_EPOCH;

use child::{Containment, PATIENCE, Running, Setup, TempDir, free_port, text};
use durchlauferhitzer_core::FileStamp;
use durchlauferhitzer_core::http::request::Persistence;
use model_http::{model_partial_head, model_range_not_satisfiable};

const SIZE: usize = 600 * 1024;
const TYPE: &str = "application/octet-stream";

/// Bytes that tell their positions apart (251 is prime).
fn pattern() -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8).collect()
}

/// A server with `big.bin` (the pattern) under `/`: the server, its
/// address, the file's bytes and stamp.
fn server(dir: &TempDir) -> (Running, SocketAddr, Vec<u8>, FileStamp) {
    let www = dir.dir("www");
    let bytes = pattern();
    let file = www.join("big.bin");
    fs::write(&file, &bytes).unwrap();
    let stamp = stamp_of(&file);
    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let conf = format!(
        "listen {addr};\nhost example.test {{\n  path / {{ static {}; }}\n}}\n",
        www.display()
    );
    let path = dir.config("ranges.conf", &conf, Containment::Switch);
    let running = Running::start(&[path.to_str().unwrap()], Setup::non_root(), addr);
    (running, addr, bytes, stamp)
}

/// The stamp the server reports for `path`: its size and mtime in
/// nanoseconds (FR-STATIC-18).
fn stamp_of(path: &Path) -> FileStamp {
    let meta = fs::metadata(path).unwrap();
    let mtime = meta.modified().unwrap().duration_since(UNIX_EPOCH).unwrap();
    FileStamp {
        size: meta.len(),
        modified_ns: u64::try_from(mtime.as_nanos()).unwrap(),
    }
}

/// `GET /big.bin` with `range` (and `Connection: close`): the head (with
/// its `Date` value) and the body.
fn get_range(addr: SocketAddr, range: &str) -> (String, String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    write!(
        stream,
        "GET /big.bin HTTP/1.1\r\nHost: example.test\r\nRange: {range}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no head end: {}", text(&response)))
        + 4;
    let head = text(&response[..end]);
    let date = head
        .split("\r\nDate: ")
        .nth(1)
        .and_then(|rest| rest.get(..29))
        .unwrap_or_else(|| panic!("no Date: {head}"))
        .to_owned();
    (head, date, response[end..].to_vec())
}

/// FR-STATIC-19: a range across the 256 KiB chunk boundary gets exactly
/// its bytes, with its `Content-Range`.
#[test]
fn range_crosses_a_chunk_boundary() {
    let dir = TempDir::new();
    let (running, addr, bytes, stamp) = server(&dir);
    let (head, date, body) = get_range(addr, "bytes=200000-500000");
    assert!(
        head.contains("\r\nContent-Range: bytes 200000-500000/614400\r\n"),
        "{head}"
    );
    let model = model_partial_head(
        TYPE,
        stamp,
        (200_000, 500_000),
        Persistence::Close,
        Some(&date),
    );
    assert_eq!(head, text(&model));
    assert!(
        body == bytes[200_000..=500_000],
        "{} body bytes",
        body.len()
    );
    running.kill();
}

/// A suffix range gets the file's last bytes; a range past the end the
/// 416 with the file's size.
#[test]
fn suffix_range_and_416() {
    let dir = TempDir::new();
    let (running, addr, bytes, stamp) = server(&dir);
    let (head, date, body) = get_range(addr, "bytes=-1000");
    let first = (SIZE - 1000) as u64;
    let model = model_partial_head(
        TYPE,
        stamp,
        (first, SIZE as u64 - 1),
        Persistence::Close,
        Some(&date),
    );
    assert_eq!(head, text(&model));
    assert!(body == bytes[SIZE - 1000..], "{} body bytes", body.len());

    let (head, date, body) = get_range(addr, "bytes=614400-");
    let model = model_range_not_satisfiable(SIZE as u64, Persistence::Close, Some(&date));
    assert_eq!(format!("{head}{}", text(&body)), text(&model));
    running.kill();
}
