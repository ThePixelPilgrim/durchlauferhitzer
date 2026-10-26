use crate::core::test_support::{GET_F, PEER, files_core, regular, request, secs, stamp};
use crate::core::{Action, ConnError, ConnId, Event, Interest};
use crate::file::{FileError, FileId, FileKind, FileMeta, FileSegment, RootId};
use crate::http::request::{Method, Persistence};
use crate::http::response::{BodyMode, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RejectReason, RequestSummary, TimeoutKind};

#[test]
fn static_roots_are_distinct_in_config_order() {
    let core = files_core();
    assert_eq!(
        core.static_roots(),
        [
            std::path::PathBuf::from("/srv/a"),
            std::path::PathBuf::from("/srv/b"),
            std::path::PathBuf::from("/srv/c"),
        ]
    );
}

#[test]
fn file_is_opened_read_sent_and_closed() {
    let mut core = files_core();
    let now = secs(0);
    let conn = ConnId(1);
    let file = FileId(1);
    let actions = request(&mut core, now, 1, GET_F);
    assert_eq!(
        actions,
        [
            Action::OpenFile {
                file,
                root: RootId(1),
                path: vec![b"f.txt".to_vec()],
            },
            Action::SetInterest {
                conn,
                interest: Interest::None
            },
        ]
    );
    assert_eq!(core.send_file(conn), None);
    let mut actions = Vec::new();
    let meta = regular(3);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::Log(Log::Response {
                conn,
                peer: PEER,
                status: Status::Ok,
                request: Some(RequestSummary {
                    method: Method::Get,
                    host: Some("a.test".to_owned()),
                    path: "/x/f.txt".to_owned(),
                }),
            }),
            Action::Readahead {
                file,
                offset: 0,
                len: 3
            },
            Action::SetInterest {
                conn,
                interest: Interest::Write
            },
        ]
    );
    let mut head = Vec::new();
    crate::http::response::render_file_head(
        "text/plain; charset=utf-8",
        3,
        Persistence::KeepAlive,
        &mut head,
    );
    assert_eq!(core.send_buffer(conn), head.as_slice());
    // Ready before the head is written: still nothing to sendfile.
    actions.clear();
    let stamp = stamp(3);
    core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);
    assert_eq!(core.send_file(conn), None);
    let len = head.len();
    core.handle(now, Event::Sent { conn, len }, &mut actions);
    assert_eq!(
        core.send_file(conn),
        Some(FileSegment {
            file,
            offset: 0,
            len: 3
        })
    );
    assert_eq!(core.next_deadline(), Some(secs(30)));
    actions.clear();
    core.handle(secs(1), Event::FileSent { conn, len: 2 }, &mut actions);
    assert!(actions.is_empty(), "{actions:?}");
    assert_eq!(
        core.send_file(conn),
        Some(FileSegment {
            file,
            offset: 2,
            len: 1
        })
    );
    assert_eq!(core.next_deadline(), Some(secs(31)));
    core.handle(secs(1), Event::FileSent { conn, len: 9 }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::CloseFile { file },
            Action::SetInterest {
                conn,
                interest: Interest::Read
            },
        ]
    );
    assert_eq!(core.send_file(conn), None);
    // The next request gets a fresh file id.
    let next = GET_F;
    let spare = core.recv_buffer(conn);
    spare.get_mut(..next.len()).unwrap().copy_from_slice(next);
    actions.clear();
    let len = next.len();
    core.handle(secs(1), Event::Received { conn, len }, &mut actions);
    assert!(
        actions.contains(&Action::OpenFile {
            file: FileId(2),
            root: RootId(1),
            path: vec![b"f.txt".to_vec()],
        }),
        "{actions:?}"
    );
}

#[test]
fn empty_file_sends_only_the_head() {
    let mut core = files_core();
    let now = secs(0);
    let conn = ConnId(1);
    let file = FileId(1);
    request(&mut core, now, 1, GET_F);
    let mut actions = Vec::new();
    let meta = regular(0);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::Readahead { .. })),
        "{actions:?}"
    );
    actions.clear();
    let len = core.send_buffer(conn).len();
    core.handle(now, Event::Sent { conn, len }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::CloseFile { file },
            Action::SetInterest {
                conn,
                interest: Interest::Read
            },
        ]
    );
}

#[test]
fn directory_without_slash_is_closed_and_redirected() {
    let mut core = files_core();
    let now = secs(0);
    let conn = ConnId(1);
    let file = FileId(1);
    request(&mut core, now, 1, GET_F);
    let mut actions = Vec::new();
    let meta = FileMeta {
        kind: FileKind::Directory,
        stamp: stamp(0),
    };
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::CloseFile { file },
            Action::Log(Log::Response {
                conn,
                peer: PEER,
                status: Status::MovedPermanently,
                request: Some(RequestSummary {
                    method: Method::Get,
                    host: Some("a.test".to_owned()),
                    path: "/x/f.txt".to_owned(),
                }),
            }),
            Action::SetInterest {
                conn,
                interest: Interest::Write
            },
        ]
    );
    let mut expected = Vec::new();
    crate::http::response::render_redirect(
        "/x/f.txt/",
        Persistence::KeepAlive,
        BodyMode::Full,
        &mut expected,
    );
    assert_eq!(core.send_buffer(conn), expected.as_slice());
    assert_eq!(core.files.len(), 0);
}

#[test]
fn directory_with_slash_opens_its_index_after_closing_it() {
    let mut core = files_core();
    let now = secs(0);
    let conn = ConnId(1);
    request(
        &mut core,
        now,
        1,
        b"GET /x/d/ HTTP/1.1\r\nHost: a.test\r\n\r\n",
    );
    let mut actions = Vec::new();
    let directory = FileMeta {
        kind: FileKind::Directory,
        stamp: stamp(0),
    };
    let file = FileId(1);
    let meta = directory;
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::CloseFile { file },
            Action::OpenFile {
                file: FileId(2),
                root: RootId(1),
                path: vec![b"d".to_vec(), b"index.html".to_vec()],
            },
        ]
    );
    assert!(core.send_buffer(conn).is_empty());
    // The index is a directory too: 404.
    actions.clear();
    let file = FileId(2);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    assert_eq!(actions.first(), Some(&Action::CloseFile { file }));
    assert!(
        actions.iter().any(|a| matches!(
            a,
            Action::Log(Log::Response {
                status: Status::NotFound,
                ..
            })
        )),
        "{actions:?}"
    );
    let mut expected = Vec::new();
    render_fixed(
        Status::NotFound,
        Persistence::KeepAlive,
        BodyMode::Full,
        &mut expected,
    );
    assert_eq!(core.send_buffer(conn), expected.as_slice());
    assert_eq!(core.files.len(), 0);
}

#[test]
fn head_of_a_file_sends_the_head_and_closes_it() {
    let mut core = files_core();
    let now = secs(0);
    let conn = ConnId(1);
    let file = FileId(1);
    request(
        &mut core,
        now,
        1,
        b"HEAD /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n",
    );
    let mut actions = Vec::new();
    let meta = regular(1 << 40);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::Log(Log::Response {
                conn,
                peer: PEER,
                status: Status::Ok,
                request: Some(RequestSummary {
                    method: Method::Head,
                    host: Some("a.test".to_owned()),
                    path: "/x/f.txt".to_owned(),
                }),
            }),
            Action::CloseFile { file },
            Action::SetInterest {
                conn,
                interest: Interest::Write
            },
        ]
    );
    let mut head = Vec::new();
    crate::http::response::render_file_head(
        "text/plain; charset=utf-8",
        1 << 40,
        Persistence::KeepAlive,
        &mut head,
    );
    assert_eq!(core.send_buffer(conn), head.as_slice());
    assert_eq!(core.send_file(conn), None);
    actions.clear();
    let len = head.len();
    core.handle(now, Event::Sent { conn, len }, &mut actions);
    assert_eq!(
        actions,
        [Action::SetInterest {
            conn,
            interest: Interest::Read
        }]
    );
    assert_eq!(core.files.len(), 0);
}

#[test]
fn file_failure_closes_the_connection_and_its_file() {
    let mut core = files_core();
    let now = secs(0);
    // Failed open: 404, nothing to close, the connection stays.
    let file = FileId(1);
    let conn = ConnId(1);
    let mut actions = Vec::new();
    for error in [
        FileError::NotFound,
        FileError::Symlink,
        FileError::PermissionDenied,
        FileError::Other,
    ] {
        let mut core = files_core();
        request(&mut core, now, 1, GET_F);
        actions.clear();
        core.handle(now, Event::FileFailed { file, error }, &mut actions);
        assert_eq!(
            actions,
            [
                Action::Log(Log::Response {
                    conn,
                    peer: PEER,
                    status: Status::NotFound,
                    request: Some(RequestSummary {
                        method: Method::Get,
                        host: Some("a.test".to_owned()),
                        path: "/x/f.txt".to_owned(),
                    }),
                }),
                Action::SetInterest {
                    conn,
                    interest: Interest::Write
                },
            ],
            "{error:?}"
        );
        assert_eq!(core.files.len(), 0);
        assert_eq!(core.counted, 1);
    }
    // Failed readahead: the open file is closed too.
    let conn = ConnId(2);
    request(&mut core, now, 2, GET_F);
    let meta = regular(3);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    actions.clear();
    let error = FileError::Other;
    core.handle(now, Event::FileFailed { file, error }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::Log(Log::FileAborted {
                conn,
                peer: PEER,
                error
            }),
            Action::CloseFile { file },
            Action::Close { conn }
        ]
    );
    assert!(core.conns.is_empty());
    assert_eq!(core.counted, 0);
    assert!(core.counted_per_ip.is_empty());
    assert_eq!(core.files.len(), 0);
}

/// `Truncated` or `Changed` cannot answer an `OpenFile`: the driver is
/// broken, so the connection is closed (nothing is open to close).
#[test]
fn impossible_open_errors_close_the_connection() {
    let now = secs(0);
    let file = FileId(1);
    let conn = ConnId(1);
    for error in [FileError::Truncated, FileError::Changed] {
        let mut core = files_core();
        request(&mut core, now, 1, GET_F);
        let mut actions = Vec::new();
        core.handle(now, Event::FileFailed { file, error }, &mut actions);
        assert_eq!(actions, [Action::Close { conn }], "{error:?}");
        assert!(core.conns.is_empty());
        assert_eq!(core.counted, 0);
        assert!(core.counted_per_ip.is_empty());
        assert_eq!(core.files.len(), 0);
    }
}

/// `send_idle_timeout` passing while the open or a readahead is in
/// flight is a disk timeout; while a ready chunk waits for the client,
/// a send timeout.
#[test]
fn disk_and_send_timeouts_while_sending_a_file() {
    let now = secs(0);
    let conn = ConnId(1);
    let file = FileId(1);
    let rejected = |kind| {
        Action::Log(Log::Rejected {
            conn,
            peer: PEER,
            reason: RejectReason::Timeout(kind),
        })
    };
    // Waiting on the open: the open becomes an orphan.
    let mut core = files_core();
    request(&mut core, now, 1, GET_F);
    let mut actions = Vec::new();
    core.handle(secs(30), Event::TimerFired, &mut actions);
    assert_eq!(
        actions,
        [rejected(TimeoutKind::Disk), Action::Close { conn }]
    );
    assert_eq!(core.counted, 1);
    // Waiting on a readahead: the file is closed.
    let mut core = files_core();
    request(&mut core, now, 1, GET_F);
    let meta = regular(3);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    let len = core.send_buffer(conn).len();
    core.handle(now, Event::Sent { conn, len }, &mut actions);
    actions.clear();
    core.handle(secs(30), Event::TimerFired, &mut actions);
    assert_eq!(
        actions,
        [
            rejected(TimeoutKind::Disk),
            Action::CloseFile { file },
            Action::Close { conn }
        ]
    );
    assert_eq!(core.counted, 0);
    assert_eq!(core.files.len(), 0);
    // A ready chunk the client does not take: a send timeout.
    let mut core = files_core();
    request(&mut core, now, 1, GET_F);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    let stamp = stamp(3);
    core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);
    actions.clear();
    core.handle(secs(30), Event::TimerFired, &mut actions);
    assert_eq!(
        actions,
        [
            rejected(TimeoutKind::Send),
            Action::CloseFile { file },
            Action::Close { conn }
        ]
    );
    assert_eq!(core.files.len(), 0);
}

#[test]
fn late_file_events_after_close() {
    let mut core = files_core();
    let now = secs(0);
    let conn = ConnId(1);
    let file = FileId(1);
    request(&mut core, now, 1, GET_F);
    let mut actions = Vec::new();
    // The open is still in flight when the send idle deadline passes.
    core.handle(secs(30), Event::TimerFired, &mut actions);
    assert!(actions.contains(&Action::Close { conn }), "{actions:?}");
    assert!(
        !actions
            .iter()
            .any(|a| matches!(a, Action::CloseFile { .. })),
        "{actions:?}"
    );
    assert_eq!(core.next_deadline(), None);
    actions.clear();
    let meta = regular(3);
    core.handle(secs(31), Event::FileOpened { file, meta }, &mut actions);
    assert_eq!(actions, [Action::CloseFile { file }]);
    // Anything later for that file, or for files never opened, is ignored.
    actions.clear();
    let stamp = stamp(3);
    for event in [
        Event::FileOpened { file, meta },
        Event::ChunkReady { file, stamp },
        Event::FileFailed {
            file,
            error: FileError::Truncated,
        },
        Event::FileOpened {
            file: FileId(9),
            meta,
        },
        Event::FileSent { conn, len: 1 },
    ] {
        core.handle(secs(31), event, &mut actions);
    }
    assert!(actions.is_empty(), "{actions:?}");

    // A connection reading a file that goes away closes its file.
    let conn = ConnId(2);
    let file = FileId(2);
    request(&mut core, now, 2, GET_F);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    actions.clear();
    core.handle(now, Event::ReadClosed { conn }, &mut actions);
    assert_eq!(
        actions,
        [Action::CloseFile { file }, Action::Close { conn }]
    );
    actions.clear();
    core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);
    assert!(actions.is_empty(), "{actions:?}");
    assert_eq!(core.files.len(), 0);
}

#[test]
fn static_rules_decided_before_opening() {
    let mut core = files_core();
    let now = secs(0);
    for (id, req, status) in [
        (
            1,
            b"HEAD /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n".as_slice(),
            None,
        ),
        (
            2,
            b"POST /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n",
            Some(Status::MethodNotAllowed),
        ),
        (
            3,
            b"GET /x/.f HTTP/1.1\r\nHost: a.test\r\n\r\n",
            Some(Status::NotFound),
        ),
        (
            4,
            b"GET /x/f HTTP/1.1\r\nHost: a.test\r\nContent-Length: 1\r\n\r\nx",
            Some(Status::ContentTooLarge),
        ),
    ] {
        let actions = request(&mut core, now, id, req);
        let opened = actions.iter().any(|a| matches!(a, Action::OpenFile { .. }));
        let logged = actions.iter().find_map(|a| {
            if let Action::Log(Log::Response { status, .. }) = a {
                Some(*status)
            } else {
                None
            }
        });
        assert_eq!(opened, status.is_none(), "{id}: {actions:?}");
        assert_eq!(logged, status, "{id}: {actions:?}");
    }
}

/// An aborted transfer and a send timeout while sending a file end the
/// same way: the reason is logged, then the file and the connection are
/// closed (stage-2 review M7).
#[test]
fn abort_and_timeout_emit_log_close_file_close() {
    let mut core = files_core();
    let now = secs(0);
    let meta = regular(3);
    // conn 1: its chunk fails to read.
    let (aborted, aborted_file) = (ConnId(1), FileId(1));
    request(&mut core, now, 1, GET_F);
    let mut actions = Vec::new();
    let file = aborted_file;
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    // conn 2: its chunk is ready, but the client does not take it.
    let (stalled, stalled_file) = (ConnId(2), FileId(2));
    request(&mut core, now, 2, GET_F);
    let file = stalled_file;
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    let stamp = stamp(3);
    core.handle(now, Event::ChunkReady { file, stamp }, &mut actions);

    actions.clear();
    let error = FileError::Other;
    let file = aborted_file;
    core.handle(now, Event::FileFailed { file, error }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::Log(Log::FileAborted {
                conn: aborted,
                peer: PEER,
                error
            }),
            Action::CloseFile { file },
            Action::Close { conn: aborted }
        ]
    );

    actions.clear();
    core.handle(secs(30), Event::TimerFired, &mut actions);
    assert_eq!(
        actions,
        [
            Action::Log(Log::Rejected {
                conn: stalled,
                peer: PEER,
                reason: RejectReason::Timeout(TimeoutKind::Send),
            }),
            Action::CloseFile { file: stalled_file },
            Action::Close { conn: stalled }
        ]
    );
    assert!(core.conns.is_empty());
    assert_eq!(core.files.len(), 0);
}

/// A reset while sending a file ends like the other teardowns: the reason is
/// logged, then the file and the connection are closed.
#[test]
fn reset_while_sending_a_file_emits_log_close_file_close() {
    let mut core = files_core();
    let now = secs(0);
    let (conn, file) = (ConnId(1), FileId(1));
    request(&mut core, now, 1, GET_F);
    let mut actions = Vec::new();
    let meta = regular(3);
    core.handle(now, Event::FileOpened { file, meta }, &mut actions);
    actions.clear();
    let error = ConnError::Reset;
    core.handle(now, Event::Failed { conn, error }, &mut actions);
    assert_eq!(
        actions,
        [
            Action::Log(Log::Reset { conn, peer: PEER }),
            Action::CloseFile { file },
            Action::Close { conn }
        ]
    );
    assert!(core.conns.is_empty());
    assert_eq!(core.files.len(), 0);
}
