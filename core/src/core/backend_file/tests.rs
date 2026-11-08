//! Backend-directed files at the core level (FR-PROXY-32 Trigger, Bad
//! path, Missing file, the client's response, the backend's body): the
//! trigger on a backend's `200`, the upstream's fate, the merged head, the
//! file's answers, no proxy limits; the rest in `flow_tests.rs`. The
//! expected heads are written out here, not taken from the core's
//! renderers. End to end in `sim/tests/proxy/backend_files.rs`.

use crate::core::test_support::{PEER, request, secs};
use crate::core::{Action, ConnId, Core, Event};
use crate::file::{FileError, FileId, FileKind, FileMeta, FileStamp, RootId};
use crate::http::request::Method;
use crate::http::status::Status;
use crate::log::{BackendFailure, Log, RequestSummary};
use crate::upstream::{BackendId, UpstreamHolder, UpstreamId};

/// `/dl` and `/grow` serve `x-accel-redirect /protected/` below
/// `/srv/files` (`/grow` with `allow_growth`), `/sf` `x-sendfile` below
/// the same root; `/plain` has no `internal_files`. Every route caps the
/// response body at 10 bytes.
pub(super) const CONFIG: &str = "listen 127.0.0.1:8080; host a.test { \
    path /dl { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10; \
      internal_files x-accel-redirect /protected/ /srv/files; } \
    path /grow { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10; \
      internal_files x-accel-redirect /protected/ /srv/files; on_file_change allow_growth; } \
    path /sf { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10; \
      internal_files x-sendfile /srv/files; } \
    path /plain { proxy 127.0.0.1:3000; max_request_body 1MiB; max_response_body 10; } }";

pub(super) const CONN: ConnId = ConnId(1);
pub(super) const UP: UpstreamId = UpstreamId(1);
pub(super) const FILE: FileId = FileId(1);
/// The internal root: the config has no static root before it.
pub(super) const ROOT: RootId = RootId(0);

pub(super) const GET: &[u8] = b"GET /dl/x HTTP/1.1\r\nHost: a.test\r\n\r\n";

/// The validators of a 1000-byte file of mtime 7 ns.
pub(super) const VALIDATORS: &str =
    "ETag: \"3e8-7\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\n";

/// A backend `200` naming `a.pdf`, with `Content-Length: 0`.
pub(super) const NAMES_A: &[u8] =
    b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 0\r\n\r\n";

pub(super) fn core() -> Core {
    Core::new(crate::config::load(CONFIG).unwrap())
}

pub(super) fn handle(core: &mut Core, event: Event) -> Vec<Action> {
    let mut actions = Vec::new();
    core.handle(secs(0), event, &mut actions);
    actions
}

pub(super) fn stamp(size: u64) -> FileStamp {
    FileStamp {
        size,
        modified_ns: 7,
    }
}

pub(super) fn regular(size: u64) -> FileMeta {
    FileMeta {
        kind: FileKind::Regular,
        stamp: stamp(size),
    }
}

/// `wire` arrives on conn 1, upstream 1 connects and takes the request
/// head and body whole.
pub(super) fn forwarded(core: &mut Core, wire: &[u8]) -> Vec<Action> {
    let mut all = request(core, secs(0), 1, wire);
    all.extend(handle(core, Event::UpstreamConnected { up: UP }));
    loop {
        let len = core.upstream_send_buffer(UP).len();
        if len == 0 {
            break;
        }
        all.extend(handle(core, Event::UpstreamSent { up: UP, len }));
    }
    all
}

/// The backend sends `bytes` (one read).
pub(super) fn answer(core: &mut Core, bytes: &[u8]) -> Vec<Action> {
    let spare = core.upstream_recv_buffer(UP);
    spare.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    handle(
        core,
        Event::UpstreamReceived {
            up: UP,
            len: bytes.len(),
        },
    )
}

/// The file opens as `meta`.
pub(super) fn opened(core: &mut Core, meta: FileMeta) -> Vec<Action> {
    handle(core, Event::FileOpened { file: FILE, meta })
}

/// `wire` forwarded, answered by `backend`, its file opened as a 1000-byte
/// regular file: every action.
pub(super) fn served(core: &mut Core, wire: &[u8], backend: &[u8]) -> Vec<Action> {
    let mut all = forwarded(core, wire);
    all.extend(answer(core, backend));
    all.extend(opened(core, regular(1000)));
    all
}

/// The client takes its whole send buffer (one write).
pub(super) fn take_head(core: &mut Core) -> Vec<Action> {
    let len = core.send_buffer(CONN).len();
    handle(core, Event::Sent { conn: CONN, len })
}

/// Answers each readahead with a chunk of `chunk` and sends each segment
/// the core offers, until none is left: the bytes sent and every action.
pub(super) fn send_file(core: &mut Core, chunk: FileStamp) -> (u64, Vec<Action>) {
    let mut all = handle(
        core,
        Event::ChunkReady {
            file: FILE,
            stamp: chunk,
        },
    );
    let mut sent = 0;
    while let Some(segment) = core.send_file(CONN) {
        sent += segment.len;
        let actions = handle(
            core,
            Event::FileSent {
                conn: CONN,
                len: segment.len,
            },
        );
        let more = actions
            .iter()
            .any(|a| matches!(a, Action::Readahead { .. }));
        all.extend(actions);
        if more {
            all.extend(handle(
                core,
                Event::ChunkReady {
                    file: FILE,
                    stamp: chunk,
                },
            ));
        }
        assert!(sent <= 1 << 30, "no end: {all:?}");
    }
    (sent, all)
}

pub(super) fn logs(actions: &[Action]) -> Vec<Log> {
    actions
        .iter()
        .filter_map(|a| {
            if let Action::Log(log) = a {
                Some(log.clone())
            } else {
                None
            }
        })
        .collect()
}

pub(super) fn summary(method: Method, path: &str) -> RequestSummary {
    RequestSummary {
        method,
        host: Some("a.test".to_owned()),
        path: path.to_owned(),
    }
}

pub(super) fn backend_file(status: Status, method: Method, path: &str) -> Log {
    Log::BackendFile {
        conn: CONN,
        peer: PEER,
        backend: BackendId(0),
        status,
        request: summary(method, path),
    }
}

pub(super) fn response(status: Status, method: Method, path: &str) -> Log {
    Log::Response {
        conn: CONN,
        peer: PEER,
        status,
        request: Some(summary(method, path)),
    }
}

pub(super) fn closes_upstream(actions: &[Action]) -> bool {
    actions.contains(&Action::CloseUpstream { up: UP })
}

pub(super) fn opens(actions: &[Action]) -> bool {
    actions.iter().any(|a| matches!(a, Action::OpenFile { .. }))
}

pub(super) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The plain 404 of FR-STATIC-09, keep-alive.
pub(super) const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain; charset=utf-8\r\n\
    Content-Length: 14\r\nX-Content-Type-Options: nosniff\r\n\r\n404 Not Found\n";

// ---- opening and the upstream --------------------------------------------

/// Trigger: the route's header on a `200` opens the named file below the
/// route's root; the backend's body is never read, nor sent.
#[test]
fn route_header_on_200_opens_the_file() {
    let mut core = core();
    forwarded(&mut core, GET);
    let actions = answer(
        &mut core,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 5\r\n\r\nhello",
    );
    assert!(
        actions.contains(&Action::OpenFile {
            file: FILE,
            root: ROOT,
            path: vec![b"a.pdf".to_vec()],
        }),
        "{actions:?}"
    );
    // The body is not read: the upstream is gone, nothing is offered.
    assert!(closes_upstream(&actions), "{actions:?}");
    assert!(core.upstream_recv_buffer(UP).is_empty());
    assert!(core.send_buffer(CONN).is_empty());
    let actions = opened(&mut core, regular(1000));
    assert!(
        !text(core.send_buffer(CONN)).contains("hello"),
        "{actions:?}"
    );
    take_head(&mut core);
    let (sent, _) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
}

/// FR-PROXY-02: the upstream is pooled only after `Content-Length: 0` on
/// a reusable exchange; any other body, a close, a HEAD or bytes behind
/// the head close it.
#[test]
fn upstream_parked_only_after_content_length_0() {
    let mut core = core();
    forwarded(&mut core, GET);
    let actions = answer(&mut core, NAMES_A);
    assert!(!closes_upstream(&actions), "{actions:?}");
    assert_eq!(core.upstream_holder(UP), Some(UpstreamHolder::Pool));
    assert!(opens(&actions), "{actions:?}");

    let named = "X-Accel-Redirect: /protected/a.pdf\r\n";
    let closing: [(&[u8], String); 6] = [
        (
            GET,
            format!("HTTP/1.1 200 OK\r\n{named}Content-Length: 5\r\n\r\n"),
        ),
        (
            GET,
            format!("HTTP/1.1 200 OK\r\n{named}Transfer-Encoding: chunked\r\n\r\n"),
        ),
        (GET, format!("HTTP/1.1 200 OK\r\n{named}\r\n")),
        (
            GET,
            format!("HTTP/1.1 200 OK\r\n{named}Connection: close\r\nContent-Length: 0\r\n\r\n"),
        ),
        (
            b"HEAD /dl/x HTTP/1.1\r\nHost: a.test\r\n\r\n",
            format!("HTTP/1.1 200 OK\r\n{named}Content-Length: 0\r\n\r\n"),
        ),
        (
            GET,
            format!("HTTP/1.1 200 OK\r\n{named}Content-Length: 0\r\n\r\nHTTP/1.1"),
        ),
    ];
    for (wire, backend) in closing {
        let mut core = self::core();
        forwarded(&mut core, wire);
        let actions = answer(&mut core, backend.as_bytes());
        assert!(closes_upstream(&actions), "{backend}: {actions:?}");
        assert_eq!(core.upstream_holder(UP), None, "{backend}");
        assert!(opens(&actions), "{backend}: {actions:?}");
    }
}

// ---- the merged head -----------------------------------------------------

/// The client's response: the core's status line, the backend's headers
/// (lowercase, in order, file headers and framing gone), then the core's
/// lines for the file; the backend's `Content-Type` wins.
#[test]
fn merged_head_keeps_backend_headers() {
    let mut core = core();
    let actions = served(
        &mut core,
        GET,
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-custom\r\n\
          Content-Disposition: attachment; filename=\"a.pdf\"\r\nCache-Control: private\r\n\
          Content-Length: 0\r\nETag: \"b\"\r\nLast-Modified: Sun, 06 Nov 1994 08:49:37 GMT\r\n\
          Accept-Ranges: none\r\nX-Accel-Redirect: /protected/a.pdf\r\n\r\n",
    );
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/x-custom\r\n\
         content-disposition: attachment; filename=\"a.pdf\"\r\ncache-control: private\r\n\
         Content-Length: 1000\r\n{VALIDATORS}Accept-Ranges: bytes\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n"
    );
    assert_eq!(text(core.send_buffer(CONN)), head, "{actions:?}");
    // The head is of its own (it may outgrow the send buffer), written in
    // pieces before the file.
    assert_eq!(core.send_capacity(CONN), core.head_capacity);
    // Even with its first chunk ready, no file byte goes before the head.
    handle(
        &mut core,
        Event::ChunkReady {
            file: FILE,
            stamp: stamp(1000),
        },
    );
    assert_eq!(core.send_file(CONN), None);
    handle(&mut core, Event::Sent { conn: CONN, len: 9 });
    assert_eq!(text(core.send_buffer(CONN)), head[9..]);
    let actions = take_head(&mut core);
    assert!(core.send_buffer(CONN).is_empty(), "{actions:?}");
    assert_eq!(core.send_capacity(CONN), 1024);
    let (sent, _) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
}

/// Without a backend `Content-Type`, the name's (FR-STATIC-12), in the
/// core's place.
#[test]
fn content_type_from_the_name_without_a_backend_one() {
    let mut core = core();
    served(
        &mut core,
        GET,
        b"HTTP/1.1 200 OK\r\nCache-Control: private\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          Content-Length: 0\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 200 OK\r\ncache-control: private\r\nContent-Type: application/pdf\r\n\
             Content-Length: 1000\r\n{VALIDATORS}Accept-Ranges: bytes\r\n\
             X-Content-Type-Options: nosniff\r\n\r\n"
        )
    );
}

/// The backend's `Date` (and `X-Content-Type-Options`) is kept in its
/// place; without one the core's follows the status line.
#[test]
fn backend_date_kept_core_date_otherwise() {
    let mut core = core();
    core.set_date(784_111_777);
    served(
        &mut core,
        GET,
        b"HTTP/1.1 200 OK\r\nDate: Mon, 01 Jan 2024 00:00:00 GMT\r\n\
          X-Content-Type-Options: NOSNIFF\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          Content-Length: 0\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 200 OK\r\ndate: Mon, 01 Jan 2024 00:00:00 GMT\r\n\
             x-content-type-options: NOSNIFF\r\nContent-Type: application/pdf\r\n\
             Content-Length: 1000\r\n{VALIDATORS}Accept-Ranges: bytes\r\n\r\n"
        )
    );
    let mut core = self::core();
    core.set_date(784_111_777);
    served(&mut core, GET, NAMES_A);
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 200 OK\r\nDate: Sun, 06 Nov 1994 08:49:37 GMT\r\n\
             Content-Type: application/pdf\r\nContent-Length: 1000\r\n{VALIDATORS}\
             Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        )
    );
}

// ---- conditionals, ranges, HEAD, methods ---------------------------------

/// FR-STATIC-18 on a backend's file: the `304` keeps the backend's headers
/// and adds only the validators; the file closes once the head is out.
#[test]
fn conditional_304_keeps_backend_headers() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"GET /dl/x HTTP/1.1\r\nHost: a.test\r\nIf-None-Match: \"3e8-7\"\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-custom\r\n\
          Content-Disposition: attachment\r\nCache-Control: private\r\n\
          X-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 304 Not Modified\r\ncontent-type: application/x-custom\r\n\
             content-disposition: attachment\r\ncache-control: private\r\n{VALIDATORS}\r\n"
        )
    );
    assert_eq!(
        logs(&actions),
        [backend_file(Status::NotModified, Method::Get, "/dl/x")]
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::Readahead { .. } | Action::CloseFile { .. })),
        "{actions:?}"
    );
    let actions = take_head(&mut core);
    assert!(
        actions.contains(&Action::CloseFile { file: FILE }),
        "{actions:?}"
    );
    assert_eq!(core.send_file(CONN), None);
    assert!(
        !core.recv_buffer(CONN).is_empty(),
        "keep-alive: {actions:?}"
    );
}

/// FR-STATIC-19 on a backend's file: a `206` with the backend's headers
/// and exactly the range's bytes.
#[test]
fn range_206_keeps_backend_headers() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"GET /dl/x HTTP/1.1\r\nHost: a.test\r\nRange: bytes=0-99\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nCache-Control: private\r\nContent-Range: bytes 0-4/5\r\n\
          X-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 0\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 206 Partial Content\r\ncache-control: private\r\n\
             Content-Type: application/pdf\r\nContent-Length: 100\r\n\
             Content-Range: bytes 0-99/1000\r\n{VALIDATORS}Accept-Ranges: bytes\r\n\
             X-Content-Type-Options: nosniff\r\n\r\n"
        )
    );
    assert!(actions.contains(&Action::Readahead {
        file: FILE,
        offset: 0,
        len: 100
    }));
    assert_eq!(
        logs(&actions),
        [backend_file(Status::PartialContent, Method::Get, "/dl/x")]
    );
    take_head(&mut core);
    let (sent, _) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 100);
}

/// A range that misses the file: the plain `416` of FR-STATIC-19, the
/// backend's headers dropped, logged as a `Response`.
#[test]
fn range_416_is_plain() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"GET /dl/x HTTP/1.1\r\nHost: a.test\r\nRange: bytes=2000-\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nCache-Control: private\r\nX-Accel-Redirect: /protected/a.pdf\r\n\
          Content-Length: 0\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Length: 26\r\nContent-Range: bytes */1000\r\nAccept-Ranges: bytes\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n416 Range Not Satisfiable\n"
    );
    assert_eq!(
        logs(&actions),
        [response(Status::RangeNotSatisfiable, Method::Get, "/dl/x")]
    );
    assert!(actions.contains(&Action::CloseFile { file: FILE }));
}

/// FR-STATIC-13: a HEAD gets the head with the file's length and no body;
/// nothing is read.
#[test]
fn head_request_gets_head_only() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"HEAD /dl/x HTTP/1.1\r\nHost: a.test\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 77\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 1000\r\n\
             {VALIDATORS}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\r\n"
        )
    );
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::Readahead { .. })),
        "{actions:?}"
    );
    assert_eq!(
        logs(&actions),
        [backend_file(Status::Ok, Method::Head, "/dl/x")]
    );
    let actions = take_head(&mut core);
    assert!(actions.contains(&Action::CloseFile { file: FILE }));
    assert_eq!(core.send_file(CONN), None);
}

/// Any method gets the file; `Range` counts for a `GET` only, so a POST
/// gets the whole `200`, and keeps its connection.
#[test]
fn post_gets_the_file_and_range_is_ignored() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"POST /dl/x HTTP/1.1\r\nHost: a.test\r\nContent-Length: 3\r\nRange: bytes=0-1\r\n\r\nabc",
        NAMES_A,
    );
    assert!(
        text(core.send_buffer(CONN)).starts_with(
            "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 1000\r\n"
        ),
        "{actions:?}"
    );
    assert_eq!(
        logs(&actions),
        [backend_file(
            Status::Ok,
            Method::Other("POST".to_owned()),
            "/dl/x"
        )]
    );
    assert!(!text(core.send_buffer(CONN)).contains("Connection: close"));
    take_head(&mut core);
    let (sent, actions) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
    assert!(actions.contains(&Action::CloseFile { file: FILE }));
    assert!(!actions.contains(&Action::ShutdownWrite { conn: CONN }));
    assert!(
        !core.recv_buffer(CONN).is_empty(),
        "keep-alive: {actions:?}"
    );
}

/// A request's conditional headers count for a `GET` and a `HEAD` only
/// (RFC 9110 section 13.1.2): a POST answered with a file gets the file.
#[test]
fn post_ignores_if_none_match() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"POST /dl/x HTTP/1.1\r\nHost: a.test\r\nContent-Length: 0\r\n\
          If-None-Match: \"3e8-7\"\r\n\r\n",
        NAMES_A,
    );
    assert_eq!(
        logs(&actions),
        [backend_file(
            Status::Ok,
            Method::Other("POST".to_owned()),
            "/dl/x"
        )]
    );
}

// ---- not found and bad paths ---------------------------------------------

/// Missing file: the plain 404, logged as a `Response` only.
#[test]
fn missing_file_is_a_plain_404() {
    let mut core = core();
    let mut all = forwarded(&mut core, GET);
    all.extend(answer(&mut core, NAMES_A));
    let actions = handle(
        &mut core,
        Event::FileFailed {
            file: FILE,
            error: FileError::NotFound,
        },
    );
    all.extend(actions);
    assert_eq!(text(core.send_buffer(CONN)), NOT_FOUND);
    assert_eq!(
        logs(&all),
        [response(Status::NotFound, Method::Get, "/dl/x")]
    );
    take_head(&mut core);
    assert!(!core.recv_buffer(CONN).is_empty(), "keep-alive");
}

/// A directory (no index is tried), a regular file named with a trailing
/// `/`, and the prefix alone (the root) are 404.
#[test]
fn directory_and_trailing_slash_are_404() {
    let directory = FileMeta {
        kind: FileKind::Directory,
        stamp: stamp(4096),
    };
    let cases: [(&str, Vec<Vec<u8>>, FileMeta); 3] = [
        ("/protected/a", vec![b"a".to_vec()], directory),
        ("/protected/a/", vec![b"a".to_vec()], regular(1000)),
        ("/protected/", vec![], directory),
    ];
    for (value, path, meta) in cases {
        let mut core = core();
        forwarded(&mut core, GET);
        let backend =
            format!("HTTP/1.1 200 OK\r\nX-Accel-Redirect: {value}\r\nContent-Length: 0\r\n\r\n");
        let actions = answer(&mut core, backend.as_bytes());
        assert!(
            actions.contains(&Action::OpenFile {
                file: FILE,
                root: ROOT,
                path
            }),
            "{value}: {actions:?}"
        );
        let actions = opened(&mut core, meta);
        assert!(!opens(&actions), "{value}: no index: {actions:?}");
        assert!(actions.contains(&Action::CloseFile { file: FILE }));
        assert_eq!(text(core.send_buffer(CONN)), NOT_FOUND, "{value}");
        assert_eq!(
            logs(&actions),
            [response(Status::NotFound, Method::Get, "/dl/x")],
            "{value}"
        );
    }
}

/// A dotfile, also a decoded one, is a 404 without an open (FR-STATIC-09).
#[test]
fn dotfile_is_404() {
    for value in ["/protected/.env", "/protected/d/%2ehidden"] {
        let mut core = core();
        let mut all = forwarded(&mut core, GET);
        let backend =
            format!("HTTP/1.1 200 OK\r\nX-Accel-Redirect: {value}\r\nContent-Length: 0\r\n\r\n");
        let actions = answer(&mut core, backend.as_bytes());
        assert!(!opens(&actions), "{value}: {actions:?}");
        all.extend(actions);
        assert_eq!(text(core.send_buffer(CONN)), NOT_FOUND, "{value}");
        assert_eq!(
            logs(&all),
            [response(Status::NotFound, Method::Get, "/dl/x")],
            "{value}"
        );
    }
}

/// Bad path: one case of each kind, and the header on two lines, is a
/// `502` logged as `BackendFailed` with `BadFilePath`; the upstream
/// closes and nothing is opened.
#[test]
fn bad_paths_are_502() {
    let accel = |value: &str| format!("X-Accel-Redirect: {value}\r\n");
    let cases: [(&[u8], String); 8] = [
        (GET, accel("")),
        (GET, accel("/protected/a b")),
        (GET, accel("/protected/a?x")),
        (GET, accel("/etc/passwd")),
        (GET, accel("/protected/a/../b")),
        (GET, accel("/protected/%zz")),
        (GET, accel("/protected/a") + &accel("/protected/a")),
        (
            b"GET /sf/x HTTP/1.1\r\nHost: a.test\r\n\r\n",
            "X-Sendfile: /srv/filesx/a\r\n".to_owned(),
        ),
    ];
    for (wire, lines) in cases {
        let mut core = core();
        let path = if wire == GET { "/dl/x" } else { "/sf/x" };
        let mut all = forwarded(&mut core, wire);
        let backend = format!("HTTP/1.1 200 OK\r\n{lines}Content-Length: 0\r\n\r\n");
        let actions = answer(&mut core, backend.as_bytes());
        all.extend(actions.clone());
        assert_eq!(
            logs(&all),
            [
                Log::BackendFailed {
                    conn: CONN,
                    peer: PEER,
                    backend: BackendId(0),
                    kind: BackendFailure::BadFilePath,
                },
                response(Status::BadGateway, Method::Get, path),
            ],
            "{lines}"
        );
        let log_at = actions
            .iter()
            .position(|a| matches!(a, Action::Log(Log::Response { .. })))
            .unwrap();
        let close_at = actions
            .iter()
            .position(|a| *a == Action::CloseUpstream { up: UP })
            .unwrap();
        assert!(log_at < close_at, "{lines}: {actions:?}");
        assert!(!opens(&actions), "{lines}: {actions:?}");
        assert!(text(core.send_buffer(CONN)).starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
    }
}

// ---- no proxy limits on the file -----------------------------------------

/// No `Transfer-Encoding` check for an HTTP/1.0 client: the chunked head's
/// framing is replaced by the file's.
#[test]
fn chunked_head_to_http10_client_is_served() {
    let mut core = core();
    let actions = served(
        &mut core,
        b"GET /dl/x HTTP/1.0\r\nHost: a.test\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX-Accel-Redirect: /protected/a.pdf\r\n\r\n",
    );
    assert_eq!(
        text(core.send_buffer(CONN)),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: 1000\r\n\
             {VALIDATORS}Accept-Ranges: bytes\r\nX-Content-Type-Options: nosniff\r\n\
             Connection: close\r\n\r\n"
        )
    );
    assert_eq!(
        logs(&actions),
        [backend_file(Status::Ok, Method::Get, "/dl/x")]
    );
}

/// `max_response_body` (10 here) caps neither the file nor the backend's
/// discarded body.
#[test]
fn max_response_body_does_not_apply() {
    let mut core = core();
    let actions = served(
        &mut core,
        GET,
        b"HTTP/1.1 200 OK\r\nX-Accel-Redirect: /protected/a.pdf\r\nContent-Length: 5000\r\n\r\n",
    );
    assert_eq!(
        logs(&actions),
        [backend_file(Status::Ok, Method::Get, "/dl/x")]
    );
    take_head(&mut core);
    let (sent, _) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
}
