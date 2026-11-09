//! Range requests on static files (FR-STATIC-19): the decision after the
//! open, the heads, and the chunks of a range. The expected heads are
//! written out here, not taken from the core's renderers.

use crate::core::test_support::{PEER, files_core, request, secs};
use crate::core::{Action, ConnId, Core, Event, Interest};
use crate::file::{FileError, FileId, FileKind, FileMeta, FileSegment, FileStamp};
use crate::http::request::Method;
use crate::http::status::Status;
use crate::log::{Log, RequestSummary};

const CONN: ConnId = ConnId(1);
const FILE: FileId = FileId(1);

/// `a.txt`'s validators at 1000 bytes (mtime 7 ns: the epoch).
const VALIDATORS: &str = "ETag: \"3e8-7\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\n";

/// The keep-alive head of the whole 1000-byte `a.txt`.
const WHOLE_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
    Content-Length: 1000\r\nETag: \"3e8-7\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\n\
    Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n";

fn stamp(size: u64) -> FileStamp {
    FileStamp {
        size,
        modified_ns: 7,
    }
}

fn meta(stamp: FileStamp) -> FileMeta {
    FileMeta {
        kind: FileKind::Regular,
        stamp,
    }
}

fn head(method: &str, path: &str, lines: &str) -> Vec<u8> {
    format!("{method} {path} HTTP/1.1\r\nHost: a.test\r\n{lines}\r\n").into_bytes()
}

fn logged(method: Method, path: &str, status: Status) -> Action {
    Action::Log(Log::Response {
        conn: CONN,
        peer: PEER,
        status,
        request: Some(RequestSummary {
            method,
            host: Some("a.test".to_owned()),
            path: path.to_owned(),
        }),
    })
}

/// `request_bytes` on `core`, its file opened with `stamp`: the actions of
/// the open.
fn open(core: &mut Core, request_bytes: &[u8], stamp: FileStamp) -> Vec<Action> {
    let actions = request(core, secs(0), 1, request_bytes);
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::OpenFile { file, .. } if *file == FILE)),
        "{actions:?}"
    );
    let mut actions = Vec::new();
    let meta = meta(stamp);
    core.handle(
        secs(0),
        Event::FileOpened { file: FILE, meta },
        &mut actions,
    );
    actions
}

fn write_interest() -> Action {
    Action::SetInterest {
        conn: CONN,
        interest: Interest::Write,
    }
}

/// Writes the send buffer, then sends the file's segments as the core
/// offers them, answering each `Readahead` with a chunk of stamp
/// `chunk_stamp`, until the core closes the file. Returns the segments
/// and the readaheads after the first.
fn send_all(core: &mut Core, chunk_stamp: FileStamp) -> (Vec<FileSegment>, Vec<Action>) {
    let mut actions = Vec::new();
    let stamp = chunk_stamp;
    core.handle(
        secs(0),
        Event::ChunkReady { file: FILE, stamp },
        &mut actions,
    );
    let len = core.send_buffer(CONN).len();
    core.handle(secs(0), Event::Sent { conn: CONN, len }, &mut actions);
    let mut segments = Vec::new();
    let mut readaheads = Vec::new();
    while let Some(segment) = core.send_file(CONN) {
        segments.push(segment);
        actions.clear();
        let len = segment.len;
        core.handle(secs(0), Event::FileSent { conn: CONN, len }, &mut actions);
        for action in &actions {
            if let Action::Readahead { .. } = action {
                readaheads.push(action.clone());
                let mut more = Vec::new();
                core.handle(secs(0), Event::ChunkReady { file: FILE, stamp }, &mut more);
            }
        }
        assert!(segments.len() < 100, "no end: {segments:?}");
    }
    assert!(
        actions.contains(&Action::CloseFile { file: FILE }),
        "{actions:?}"
    );
    (segments, readaheads)
}

/// FR-STATIC-19: one satisfiable range gets a 206 with its part's length
/// and `Content-Range`, and only its bytes are read and sent.
#[test]
fn range_gets_206_with_content_range() {
    let mut core = files_core();
    let actions = open(
        &mut core,
        &head("GET", "/x/a.txt", "Range: bytes=0-99\r\n"),
        stamp(1000),
    );
    assert_eq!(
        actions,
        [
            logged(Method::Get, "/x/a.txt", Status::PartialContent),
            Action::Readahead {
                file: FILE,
                offset: 0,
                len: 100
            },
            write_interest(),
        ]
    );
    assert_eq!(
        String::from_utf8_lossy(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 100\r\nContent-Range: bytes 0-99/1000\r\n{VALIDATORS}\
             Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        )
    );
    let (segments, readaheads) = send_all(&mut core, stamp(1000));
    assert_eq!(
        segments,
        [FileSegment {
            file: FILE,
            offset: 0,
            len: 100
        }]
    );
    assert!(readaheads.is_empty(), "{readaheads:?}");
}

/// A range of more than one chunk is read from its first byte, chunk by
/// chunk, up to its last byte.
#[test]
fn range_across_chunks_reads_from_its_first_byte() {
    let mut core = files_core();
    let size = 600 * 1024;
    let actions = open(
        &mut core,
        &head("GET", "/x/a.txt", "Range: bytes=200000-500000\r\n"),
        stamp(size),
    );
    assert!(
        actions.contains(&Action::Readahead {
            file: FILE,
            offset: 200_000,
            len: 262_144
        }),
        "{actions:?}"
    );
    let (segments, readaheads) = send_all(&mut core, stamp(size));
    assert_eq!(
        readaheads,
        [Action::Readahead {
            file: FILE,
            offset: 462_144,
            len: 37_857
        }]
    );
    assert_eq!(segments.first().map(|s| s.offset), Some(200_000));
    assert_eq!(segments.iter().map(|s| s.len).sum::<u64>(), 300_001);
}

/// A range that does not overlap the file gets the 416 at once; the file
/// is closed unread and the connection kept alive.
#[test]
fn unsatisfiable_range_gets_416() {
    let mut core = files_core();
    let actions = open(
        &mut core,
        &head("GET", "/x/a.txt", "Range: bytes=1000-\r\n"),
        stamp(1000),
    );
    assert_eq!(
        actions,
        [
            logged(Method::Get, "/x/a.txt", Status::RangeNotSatisfiable),
            Action::CloseFile { file: FILE },
            write_interest(),
        ]
    );
    assert_eq!(
        String::from_utf8_lossy(core.send_buffer(CONN)),
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: 26\r\nContent-Range: bytes */1000\r\nAccept-Ranges: bytes\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n416 Range Not Satisfiable\n"
    );
    assert_eq!(core.send_file(CONN), None);
    let mut actions = Vec::new();
    let len = core.send_buffer(CONN).len();
    core.handle(secs(0), Event::Sent { conn: CONN, len }, &mut actions);
    assert_eq!(
        actions,
        [Action::SetInterest {
            conn: CONN,
            interest: Interest::Read
        }]
    );
}

/// Owner decision 2026-11-09 (FR-STATIC-19): an empty file's `Range` is
/// ignored: the whole (empty) file with `200`, exactly as without one.
#[test]
fn range_on_an_empty_file_is_a_whole_200() {
    let mut plain = files_core();
    let whole = open(&mut plain, &head("GET", "/x/a.txt", ""), stamp(0));
    for range in ["bytes=-5", "bytes=0-", "bytes=-0"] {
        let mut core = files_core();
        let actions = open(
            &mut core,
            &head("GET", "/x/a.txt", &format!("Range: {range}\r\n")),
            stamp(0),
        );
        assert!(
            actions.contains(&logged(Method::Get, "/x/a.txt", Status::Ok)),
            "{range} {actions:?}"
        );
        assert_eq!(actions, whole, "{range}");
        assert_eq!(
            String::from_utf8_lossy(core.send_buffer(CONN)),
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 0\r\nETag: \"0-7\"\r\n\
             Last-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\nAccept-Ranges: bytes\r\n\
             X-Content-Type-Options: nosniff\r\n\r\n",
            "{range}"
        );
    }
}

/// Every 200 says the server serves ranges.
#[test]
fn whole_200_carries_accept_ranges() {
    let mut core = files_core();
    let actions = open(&mut core, &head("GET", "/x/a.txt", ""), stamp(1000));
    assert!(
        actions.contains(&logged(Method::Get, "/x/a.txt", Status::Ok)),
        "{actions:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
             Content-Length: 1000\r\n{VALIDATORS}Accept-Ranges: bytes\r\n\
             X-Content-Type-Options: nosniff\r\n\r\n"
        )
    );
}

/// The conditional headers are judged first: a 304 wins over a range.
#[test]
fn not_modified_wins_over_range() {
    let mut core = files_core();
    let actions = open(
        &mut core,
        &head(
            "GET",
            "/x/a.txt",
            "If-None-Match: \"3e8-7\"\r\nRange: bytes=0-99\r\n",
        ),
        stamp(1000),
    );
    assert_eq!(
        actions,
        [
            logged(Method::Get, "/x/a.txt", Status::NotModified),
            Action::CloseFile { file: FILE },
            write_interest(),
        ]
    );
    assert_eq!(
        String::from_utf8_lossy(core.send_buffer(CONN)),
        format!("HTTP/1.1 304 Not Modified\r\n{VALIDATORS}\r\n")
    );
}

/// An `If-Range` naming the file as it was before it changed sends the
/// whole file as it is now.
#[test]
fn changed_file_ignores_if_range() {
    let mut core = files_core();
    let actions = open(
        &mut core,
        &head(
            "GET",
            "/x/a.txt",
            "Range: bytes=0-99\r\nIf-Range: \"3e8-6\"\r\n",
        ),
        stamp(1000),
    );
    assert_eq!(
        actions,
        [
            logged(Method::Get, "/x/a.txt", Status::Ok),
            Action::Readahead {
                file: FILE,
                offset: 0,
                len: 1000
            },
            write_interest(),
        ]
    );
    assert_eq!(String::from_utf8_lossy(core.send_buffer(CONN)), WHOLE_HEAD);
}

/// `Range` counts only for `GET`: a `HEAD` gets the whole file's head.
#[test]
fn range_on_head_is_a_whole_head() {
    let mut core = files_core();
    let actions = open(
        &mut core,
        &head("HEAD", "/x/a.txt", "Range: bytes=0-99\r\n"),
        stamp(1000),
    );
    assert_eq!(
        actions,
        [
            logged(Method::Head, "/x/a.txt", Status::Ok),
            Action::CloseFile { file: FILE },
            write_interest(),
        ]
    );
    assert_eq!(String::from_utf8_lossy(core.send_buffer(CONN)), WHOLE_HEAD);
}

/// A failure while a range is sent cuts it like a whole file
/// (FR-STATIC-15): the file shrank below the range's end.
#[test]
fn range_file_shrinks_mid_transfer_is_cut() {
    let mut core = files_core();
    open(
        &mut core,
        &head("GET", "/x/a.txt", "Range: bytes=100-199\r\n"),
        stamp(1000),
    );
    let mut actions = Vec::new();
    let stamp = stamp(1000);
    core.handle(
        secs(0),
        Event::ChunkReady { file: FILE, stamp },
        &mut actions,
    );
    let len = core.send_buffer(CONN).len();
    core.handle(secs(0), Event::Sent { conn: CONN, len }, &mut actions);
    assert_eq!(
        core.send_file(CONN),
        Some(FileSegment {
            file: FILE,
            offset: 100,
            len: 100
        })
    );
    core.handle(
        secs(0),
        Event::FileSent {
            conn: CONN,
            len: 50,
        },
        &mut actions,
    );
    actions.clear();
    let error = FileError::Truncated;
    core.handle(
        secs(0),
        Event::FileFailed { file: FILE, error },
        &mut actions,
    );
    assert_eq!(
        actions,
        [
            Action::Log(Log::FileAborted {
                conn: CONN,
                peer: PEER,
                error
            }),
            Action::CloseFile { file: FILE },
            Action::Close { conn: CONN },
        ]
    );
}

/// Under `allow_growth` a file that grows while its range is sent still
/// sends exactly the range (FR-STATIC-14).
#[test]
fn range_under_allow_growth_sends_exactly_the_range() {
    let config = "listen 127.0.0.1:8080; \
        host a.test { path /g { static /srv/a; on_file_change allow_growth; } }";
    let mut core = Core::new(crate::config::load(config).unwrap());
    let size = 600 * 1024;
    let actions = open(
        &mut core,
        &head("GET", "/g/a.txt", "Range: bytes=100000-400000\r\n"),
        stamp(size),
    );
    assert!(
        actions.contains(&logged(Method::Get, "/g/a.txt", Status::PartialContent)),
        "{actions:?}"
    );
    let grown = FileStamp {
        size: size + 5000,
        modified_ns: 9,
    };
    let (segments, readaheads) = send_all(&mut core, grown);
    assert_eq!(
        readaheads,
        [Action::Readahead {
            file: FILE,
            offset: 362_144,
            len: 37_857
        }]
    );
    assert_eq!(segments.first().map(|s| s.offset), Some(100_000));
    let last = segments.last().map(|s| s.offset + s.len);
    assert_eq!(last, Some(400_001));
    assert_eq!(segments.iter().map(|s| s.len).sum::<u64>(), 300_001);
}
