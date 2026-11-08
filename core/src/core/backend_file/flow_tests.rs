//! Backend-directed files at the core level, continued (FR-PROXY-32): the
//! log entry, `on_file_change`, the bounds of the merged head (NFR-RES-01,
//! -03), a `100 Continue` before it, and Review Focus 5 (drain, abort,
//! reset, pipelining). The helpers are those of `tests.rs`.

use crate::core::test_support::{PEER, request, secs};
use crate::core::{Action, DrainTarget, Event};
use crate::file::{FileError, FileStamp};
use crate::http::forward::{Kept, Sent};
use crate::http::request::Method;
use crate::http::response::{FILE_HEAD_EXTRA, FileStatus, Part, render_backend_file_head};
use crate::http::status::Status;
use crate::log::{Log, RejectReason};
use crate::upstream::UpstreamHolder;

use super::fits_backend_file;
use super::tests::{
    CONN, FILE, GET, NAMES_A, UP, answer, backend_file, core, forwarded, handle, logs, opened,
    regular, send_file, served, stamp, take_head, text,
};

// ---- logging and file changes --------------------------------------------

/// A served file is logged once, as its own kind; no `ProxiedResponse`.
#[test]
fn served_file_logged_once() {
    let mut core = core();
    let mut all = served(&mut core, GET, NAMES_A);
    all.extend(take_head(&mut core));
    let (_, actions) = send_file(&mut core, stamp(1000));
    all.extend(actions);
    assert_eq!(logs(&all), [backend_file(Status::Ok, Method::Get, "/dl/x")]);
}

/// `on_file_change` is the route's `internal_files` setting: `abort` cuts
/// a file that changes mid-transfer; `allow_growth` sends exactly the
/// size seen at open from a file that grew.
#[test]
fn on_file_change_from_internal_files() {
    let big = 300 * 1024;
    let grown = FileStamp {
        size: big + 10,
        modified_ns: 8,
    };
    // abort: the second chunk sees the change.
    let mut core = core();
    forwarded(&mut core, GET);
    answer(&mut core, NAMES_A);
    opened(&mut core, regular(big));
    take_head(&mut core);
    handle(
        &mut core,
        Event::ChunkReady {
            file: FILE,
            stamp: stamp(big),
        },
    );
    let segment = core.send_file(CONN).unwrap();
    let actions = handle(
        &mut core,
        Event::FileSent {
            conn: CONN,
            len: segment.len,
        },
    );
    assert!(
        actions
            .iter()
            .any(|a| matches!(a, Action::Readahead { .. })),
        "{actions:?}"
    );
    let actions = handle(
        &mut core,
        Event::ChunkReady {
            file: FILE,
            stamp: grown,
        },
    );
    assert_eq!(
        actions,
        [
            Action::Log(Log::FileAborted {
                conn: CONN,
                peer: PEER,
                error: FileError::Changed,
            }),
            Action::CloseFile { file: FILE },
            Action::Close { conn: CONN },
        ]
    );
    // allow_growth: every chunk sees growth; exactly the size at open goes.
    let mut core = self::core();
    forwarded(&mut core, b"GET /grow/x HTTP/1.1\r\nHost: a.test\r\n\r\n");
    answer(&mut core, NAMES_A);
    opened(&mut core, regular(big));
    take_head(&mut core);
    let (sent, actions) = send_file(&mut core, grown);
    assert_eq!(sent, big);
    assert!(actions.contains(&Action::CloseFile { file: FILE }));
}

// ---- bounds ----------------------------------------------------------------

/// NFR-RES-03: the merged head's bound, judged before the open: kept lines
/// of `capacity - FILE_HEAD_EXTRA` bytes fit, one more does not.
#[test]
fn merged_head_bound_is_502() {
    let capacity = 16_384 + 100 + 128 + FILE_HEAD_EXTRA;
    assert!(fits_backend_file(capacity - FILE_HEAD_EXTRA, capacity));
    assert!(!fits_backend_file(capacity - FILE_HEAD_EXTRA + 1, capacity));
    assert!(!fits_backend_file(usize::MAX, capacity));
}

/// `FILE_HEAD_EXTRA` bounds the core's lines: the worst `206` (`u64::MAX`
/// range and size, the longest type, a `Date`, close) around no kept
/// lines, the worst `304`, and a whole file.
#[test]
fn file_head_extra_bounds_the_core_lines() {
    let none = Kept {
        content_type: Sent::No,
        nosniff: Sent::No,
        date: Sent::No,
    };
    let validators = crate::http::conditional::Validators::of(FileStamp {
        size: u64::MAX,
        modified_ns: u64::MAX,
    });
    let date = crate::http::date::imf_fixdate(253_402_300_799);
    let parts = [
        FileStatus::File(Part::Range {
            first: 10_000_000_000_000_000_000,
            last: u64::MAX - 1,
            size: u64::MAX,
        }),
        FileStatus::File(Part::Range {
            first: 0,
            last: u64::MAX - 1,
            size: u64::MAX,
        }),
        FileStatus::File(Part::Whole { size: u64::MAX }),
        FileStatus::NotModified,
    ];
    let mut longest = 0;
    for status in parts {
        let mut head = Vec::new();
        render_backend_file_head(
            status,
            &mut head,
            none,
            "text/javascript; charset=utf-8",
            &validators,
            crate::http::request::Persistence::Close,
            Some(&date),
        );
        assert!(head.len() <= FILE_HEAD_EXTRA, "{}", text(&head));
        longest = longest.max(head.len());
    }
    // Within a few bytes of the bound: a 206 cannot carry three 20-digit
    // numbers at once (its length is below its size).
    assert!(longest + 3 >= FILE_HEAD_EXTRA, "{longest}");
    // Kept lines are moved behind the status line and `Date`, whole.
    let mut head = b"x-a: 1\r\n".to_vec();
    render_backend_file_head(
        FileStatus::NotModified,
        &mut head,
        none,
        "",
        &validators,
        crate::http::request::Persistence::KeepAlive,
        Some(&date),
    );
    assert!(
        text(&head).starts_with(
            "HTTP/1.1 304 Not Modified\r\nDate: Fri, 31 Dec 9999 23:59:59 GMT\r\nx-a: 1\r\nETag: "
        ),
        "{}",
        text(&head)
    );
}

/// A `100 Continue` the client has not taken goes out before the merged
/// head (FR-PROXY-25), and the head before the file.
#[test]
fn continue_goes_before_the_merged_head() {
    let mut core = core();
    request(
        &mut core,
        secs(0),
        1,
        b"POST /dl/x HTTP/1.1\r\nHost: a.test\r\nContent-Length: 1\r\nExpect: 100-continue\r\n\r\n",
    );
    handle(&mut core, Event::UpstreamConnected { up: UP });
    assert_eq!(
        text(core.send_buffer(CONN)),
        "HTTP/1.1 100 Continue\r\n\r\n"
    );
    let len = core.upstream_send_buffer(UP).len();
    handle(&mut core, Event::UpstreamSent { up: UP, len });
    // The backend answers before the body: the client's body stays unread,
    // and the upstream, with part of a request, is closed (FR-PROXY-02).
    let actions = answer(&mut core, NAMES_A);
    assert!(
        actions.contains(&Action::CloseUpstream { up: UP }),
        "{actions:?}"
    );
    opened(&mut core, regular(1000));
    assert_eq!(
        text(core.send_buffer(CONN)),
        "HTTP/1.1 100 Continue\r\n\r\n"
    );
    assert_eq!(core.send_capacity(CONN), 1024);
    assert_eq!(core.send_file(CONN), None);
    handle(
        &mut core,
        Event::Sent {
            conn: CONN,
            len: 25,
        },
    );
    assert!(
        text(core.send_buffer(CONN)).starts_with("HTTP/1.1 200 OK\r\n"),
        "{}",
        text(core.send_buffer(CONN))
    );
    // The body was not read: the connection closes after the file.
    assert!(text(core.send_buffer(CONN)).contains("Connection: close\r\n"));
    take_head(&mut core);
    let (sent, actions) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
    assert!(actions.contains(&Action::ShutdownWrite { conn: CONN }));
}

// ---- Review Focus 5 ------------------------------------------------------

/// A shutdown drain lets a backend file being sent finish, then closes.
#[test]
fn shutdown_drain_finishes_a_sending_backend_file() {
    let mut core = core();
    served(&mut core, GET, NAMES_A);
    let mut actions = Vec::new();
    core.begin_drain(secs(0), DrainTarget::Shutdown, &mut actions);
    assert!(
        !actions.contains(&Action::Close { conn: CONN }),
        "{actions:?}"
    );
    take_head(&mut core);
    let (sent, actions) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
    assert!(actions.contains(&Action::CloseFile { file: FILE }));
    assert!(actions.contains(&Action::ShutdownWrite { conn: CONN }));
}

/// The fast shutdown cuts a backend file being opened: logged, closed;
/// the late open is closed as an orphan.
#[test]
fn abort_all_cuts_an_opening_backend_file() {
    let mut core = core();
    forwarded(&mut core, GET);
    answer(&mut core, NAMES_A);
    let mut actions = Vec::new();
    core.abort_all(&mut actions);
    assert_eq!(
        actions,
        [
            Action::CloseUpstream { up: UP },
            Action::Log(Log::Rejected {
                conn: CONN,
                peer: PEER,
                reason: RejectReason::ShuttingDown,
            }),
            Action::Close { conn: CONN },
        ]
    );
    assert!(!core.is_drained());
    let actions = opened(&mut core, regular(1000));
    assert_eq!(actions, [Action::CloseFile { file: FILE }]);
    assert!(core.is_drained());
}

/// A client reset mid-file closes the file once; late events change
/// nothing.
#[test]
fn client_reset_mid_backend_file_closes_the_file_once() {
    let mut core = core();
    served(&mut core, GET, NAMES_A);
    take_head(&mut core);
    handle(
        &mut core,
        Event::ChunkReady {
            file: FILE,
            stamp: stamp(1000),
        },
    );
    let actions = handle(
        &mut core,
        Event::Failed {
            conn: CONN,
            error: crate::core::ConnError::Reset,
        },
    );
    let closes = actions
        .iter()
        .filter(|a| **a == Action::CloseFile { file: FILE })
        .count();
    assert_eq!(closes, 1, "{actions:?}");
    assert!(actions.contains(&Action::Close { conn: CONN }));
    let late = handle(
        &mut core,
        Event::FileSent {
            conn: CONN,
            len: 10,
        },
    );
    assert!(late.is_empty(), "{late:?}");
    let late = handle(
        &mut core,
        Event::ChunkReady {
            file: FILE,
            stamp: stamp(1000),
        },
    );
    assert!(late.is_empty(), "{late:?}");
}

/// A request pipelined behind a backend file waits until the file is out,
/// then is answered.
#[test]
fn pipelined_request_behind_a_backend_file_is_answered_after_it() {
    let mut core = core();
    let mut wire = GET.to_vec();
    wire.extend_from_slice(b"GET /plain/y HTTP/1.1\r\nHost: a.test\r\n\r\n");
    let mut all = forwarded(&mut core, &wire);
    all.extend(answer(&mut core, NAMES_A));
    all.extend(opened(&mut core, regular(1000)));
    all.extend(take_head(&mut core));
    let next = |actions: &[Action]| {
        actions
            .iter()
            .any(|a| matches!(a, Action::Connect { .. } | Action::CheckSilent { .. }))
    };
    let connects = all
        .iter()
        .filter(|a| matches!(a, Action::Connect { .. }))
        .count();
    assert_eq!(connects, 1, "only the first request so far: {all:?}");
    assert!(!all.iter().any(|a| matches!(a, Action::CheckSilent { .. })));
    let (sent, actions) = send_file(&mut core, stamp(1000));
    assert_eq!(sent, 1000);
    assert!(next(&actions), "the second request goes out: {actions:?}");
    // It reuses the parked upstream.
    assert!(
        actions.contains(&Action::CheckSilent { up: UP }),
        "{actions:?}"
    );
    assert_eq!(core.upstream_holder(UP), Some(UpstreamHolder::Conn(CONN)));
}
