//! Conditional requests on static files (FR-STATIC-18). The expected heads
//! are written out here, not taken from the core's renderers.

use crate::core::test_support::{PEER, files_core, request, secs};
use crate::core::{Action, ConnId, Core, Event, Interest};
use crate::file::{FileId, FileKind, FileMeta, FileStamp, RootId};
use crate::http::request::Method;
use crate::http::status::Status;
use crate::log::{Log, RequestSummary};

/// 2023-11-14 22:13:20 UTC plus 5 ns.
const STAMP: FileStamp = FileStamp {
    size: 3,
    modified_ns: 1_700_000_000_000_000_005,
};
const TAG: &str = "\"3-17979cfe362a0005\"";
const LAST_MODIFIED: &str = "Tue, 14 Nov 2023 22:13:20 GMT";
/// The core's clock: an hour after the mtime.
const NOW: u64 = 1_700_003_600;
const DATE: &str = "Tue, 14 Nov 2023 23:13:20 GMT";

fn meta(stamp: FileStamp) -> FileMeta {
    FileMeta {
        kind: FileKind::Regular,
        stamp,
    }
}

fn get(method: &str, headers: &str) -> Vec<u8> {
    format!("{method} /x/f.txt HTTP/1.1\r\nHost: a.test\r\n{headers}\r\n").into_bytes()
}

fn validators(stamp: FileStamp) -> String {
    let secs = stamp.modified_ns / 1_000_000_000;
    let date = match secs {
        1_700_000_000 => LAST_MODIFIED,
        1_700_000_001 => "Tue, 14 Nov 2023 22:13:21 GMT",
        _ => panic!("no date written out for {secs}"),
    };
    format!(
        "ETag: \"{:x}-{:x}\"\r\nLast-Modified: {date}\r\n",
        stamp.size, stamp.modified_ns
    )
}

fn not_modified(stamp: FileStamp, close: bool) -> String {
    let close = if close { "Connection: close\r\n" } else { "" };
    format!(
        "HTTP/1.1 304 Not Modified\r\nDate: {DATE}\r\n{}{close}\r\n",
        validators(stamp)
    )
}

fn ok_head(stamp: FileStamp) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nDate: {DATE}\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n{}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        stamp.size,
        validators(stamp)
    )
}

fn logged(method: Method, status: Status) -> Action {
    Action::Log(Log::Response {
        conn: ConnId(1),
        peer: PEER,
        status,
        request: Some(RequestSummary {
            method,
            host: Some("a.test".to_owned()),
            path: "/x/f.txt".to_owned(),
        }),
    })
}

/// `request` on a dated core, its file opened with `stamp`: the actions of
/// the open and the core.
fn answer(request_bytes: &[u8], stamp: FileStamp, clock: Option<u64>) -> (Vec<Action>, Core) {
    let mut core = files_core();
    if let Some(clock) = clock {
        core.set_date(clock);
    }
    let actions = request(&mut core, secs(0), 1, request_bytes);
    assert!(
        actions.contains(&Action::OpenFile {
            file: FileId(1),
            root: RootId(1),
            path: vec![b"f.txt".to_vec()],
        }),
        "{actions:?}"
    );
    let mut actions = Vec::new();
    let file = FileId(1);
    let meta = meta(stamp);
    core.handle(secs(0), Event::FileOpened { file, meta }, &mut actions);
    (actions, core)
}

fn assert_not_modified(request_bytes: &[u8], method: Method) {
    let (actions, core) = answer(request_bytes, STAMP, Some(NOW));
    assert_eq!(
        actions,
        [
            logged(method, Status::NotModified),
            Action::CloseFile { file: FileId(1) },
            Action::SetInterest {
                conn: ConnId(1),
                interest: Interest::Write
            },
        ],
        "{}",
        String::from_utf8_lossy(request_bytes)
    );
    assert_eq!(
        String::from_utf8_lossy(core.send_buffer(ConnId(1))),
        not_modified(STAMP, false)
    );
    assert_eq!(core.send_file(ConnId(1)), None);
}

fn assert_ok(request_bytes: &[u8], stamp: FileStamp, clock: Option<u64>) {
    let (actions, core) = answer(request_bytes, stamp, clock);
    assert_eq!(
        actions,
        [
            logged(Method::Get, Status::Ok),
            Action::Readahead {
                file: FileId(1),
                offset: 0,
                len: stamp.size,
            },
            Action::SetInterest {
                conn: ConnId(1),
                interest: Interest::Write
            },
        ],
        "{}",
        String::from_utf8_lossy(request_bytes)
    );
    let date = match clock {
        Some(_) => format!("Date: {DATE}\r\n"),
        None => String::new(),
    };
    let head = ok_head(stamp).replace(&format!("Date: {DATE}\r\n"), &date);
    assert_eq!(String::from_utf8_lossy(core.send_buffer(ConnId(1))), head);
}

/// FR-STATIC-11: every 200 carries `ETag` and `Last-Modified` after
/// `Content-Length`.
#[test]
fn a_200_carries_etag_and_last_modified() {
    assert_ok(&get("GET", ""), STAMP, Some(NOW));
    assert_ok(&get("GET", ""), STAMP, None);
}

#[test]
fn a_matching_if_none_match_is_a_304_without_reading_the_file() {
    for headers in [
        format!("If-None-Match: {TAG}\r\n"),
        format!("If-None-Match: W/{TAG}\r\n"),
        format!("If-None-Match: \"a\", {TAG}\r\n"),
        format!("If-None-Match: \"a\"\r\nIf-None-Match: W/{TAG}\r\n"),
        "If-None-Match: *\r\n".to_owned(),
    ] {
        assert_not_modified(&get("GET", &headers), Method::Get);
        assert_not_modified(&get("HEAD", &headers), Method::Head);
    }
}

#[test]
fn a_non_matching_or_malformed_if_none_match_is_a_200() {
    for headers in [
        "If-None-Match: \"3-17979cfe362a0006\"\r\n",
        "If-None-Match: \"x\"\r\n",
        "If-None-Match: 3-17979cfe362a0005\r\n",
        "If-None-Match: *\r\nIf-None-Match: \"x\"\r\n",
        // Ignored altogether.
        "If-Match: \"x\"\r\nIf-Unmodified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n",
        "If-Range: \"x\"\r\n",
    ] {
        assert_ok(&get("GET", headers), STAMP, Some(NOW));
    }
}

#[test]
fn if_modified_since_compares_the_mtime_in_seconds() {
    for date in [
        LAST_MODIFIED,
        "Tuesday, 14-Nov-23 22:13:20 GMT",
        "Tue Nov 14 22:13:20 2023",
        // The core's own time is not in the future.
        DATE,
    ] {
        let headers = format!("If-Modified-Since: {date}\r\n");
        assert_not_modified(&get("GET", &headers), Method::Get);
        assert_not_modified(&get("HEAD", &headers), Method::Head);
    }
    for date in [
        "Tue, 14 Nov 2023 22:13:19 GMT",
        // In the future: invalid.
        "Tue, 14 Nov 2023 23:13:21 GMT",
        "Tue, 14 Nov 2023 22:13:20 UTC",
    ] {
        let headers = format!("If-Modified-Since: {date}\r\n");
        assert_ok(&get("GET", &headers), STAMP, Some(NOW));
    }
    let twice =
        format!("If-Modified-Since: {LAST_MODIFIED}\r\nIf-Modified-Since: {LAST_MODIFIED}\r\n");
    assert_ok(&get("GET", &twice), STAMP, Some(NOW));
    // Before the driver set the time no date can be judged.
    let headers = format!("If-Modified-Since: {LAST_MODIFIED}\r\n");
    assert_ok(&get("GET", &headers), STAMP, None);
}

#[test]
fn if_none_match_decides_over_if_modified_since() {
    let headers = format!("If-None-Match: \"x\"\r\nIf-Modified-Since: {LAST_MODIFIED}\r\n");
    assert_ok(&get("GET", &headers), STAMP, Some(NOW));
    let headers =
        format!("If-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\nIf-None-Match: {TAG}\r\n");
    assert_not_modified(&get("GET", &headers), Method::Get); // A malformed or empty If-None-Match is present too: the date that
    // would match is ignored (RFC 9110 section 13.1.3).
    for inm in ["x", ",", "*, \"x\""] {
        let headers = format!("If-None-Match: {inm}\r\nIf-Modified-Since: {LAST_MODIFIED}\r\n");
        assert_ok(&get("GET", &headers), STAMP, Some(NOW));
    }
}

/// Review focus 2: the decision uses the stamp of the opened descriptor.
/// The client's tag names the old version; the file changed before the
/// open: a 200 with the new `ETag`.
#[test]
fn a_file_changed_before_the_open_is_a_200_with_the_new_etag() {
    let changed = FileStamp {
        size: 3,
        modified_ns: 1_700_000_001_000_000_000,
    };
    let headers = format!("If-None-Match: {TAG}\r\n");
    assert_ok(&get("GET", &headers), changed, Some(NOW));
    let headers = format!("If-Modified-Since: {LAST_MODIFIED}\r\n");
    assert_ok(&get("GET", &headers), changed, Some(NOW));
}

/// A closing 304 says so and is followed by the close.
#[test]
fn a_closing_304_says_close() {
    let headers = format!("If-None-Match: {TAG}\r\nConnection: close\r\n");
    let (actions, mut core) = answer(&get("GET", &headers), STAMP, Some(NOW));
    assert!(actions.contains(&Action::CloseFile { file: FileId(1) }));
    let sent = core.send_buffer(ConnId(1)).to_vec();
    assert_eq!(String::from_utf8_lossy(&sent), not_modified(STAMP, true));
    let mut actions = Vec::new();
    let (conn, len) = (ConnId(1), sent.len());
    core.handle(secs(0), Event::Sent { conn, len }, &mut actions);
    assert!(
        actions.contains(&Action::ShutdownWrite { conn: ConnId(1) }),
        "{actions:?}"
    );
}

/// After a 304 on a keep-alive connection the next request is served.
#[test]
fn the_connection_is_reused_after_a_304() {
    let headers = format!("If-None-Match: {TAG}\r\n");
    let (_, mut core) = answer(&get("GET", &headers), STAMP, Some(NOW));
    // NFR-PERF-06: a 304 has no body, so its head is never held back.
    assert_eq!(core.file_follows(ConnId(1)), crate::file::FileFollows::No);
    let sent = core.send_buffer(ConnId(1)).len();
    let mut actions = Vec::new();
    let conn = ConnId(1);
    core.handle(secs(0), Event::Sent { conn, len: sent }, &mut actions);
    assert_eq!(
        actions,
        [Action::SetInterest {
            conn,
            interest: Interest::Read
        }]
    );
}

/// Errors are not conditional: a missing file is a 404 even for `*`.
#[test]
fn errors_ignore_the_conditionals() {
    let mut core = files_core();
    core.set_date(NOW);
    request(&mut core, secs(0), 1, &get("GET", "If-None-Match: *\r\n"));
    let mut actions = Vec::new();
    let (file, error) = (FileId(1), crate::file::FileError::NotFound);
    core.handle(secs(0), Event::FileFailed { file, error }, &mut actions);
    assert!(
        actions.contains(&logged(Method::Get, Status::NotFound)),
        "{actions:?}"
    );
}
