//! The two drain targets (FR-PROC-05, FR-PROC-07): each restart case
//! next to its shutdown twin, which keeps the behaviour of stage 4.

use crate::core::test_support::{GET_F, PEER, accept, files_core, regular, secs, stamp};
use crate::core::{Action, ConnId, Core, DrainTarget, Event};
use crate::file::{FileId, RootId};
use crate::http::request::{Method, Persistence};
use crate::http::response::{BodyMode, render_file_head, render_fixed};
use crate::http::status::Status;
use crate::log::{Log, RejectReason, RequestSummary};
use crate::time::Instant;

/// `f.txt` is 3 bytes.
const SIZE: u64 = 3;
const TEXT: &str = "text/plain; charset=utf-8";

/// Copy `bytes` into `conn`'s receive buffer and report them read.
fn deliver(core: &mut Core, now: Instant, conn: ConnId, bytes: &[u8]) -> Vec<Action> {
    let spare = core.recv_buffer(conn);
    spare.get_mut(..bytes.len()).unwrap().copy_from_slice(bytes);
    let mut actions = Vec::new();
    let len = bytes.len();
    core.handle(now, Event::Received { conn, len }, &mut actions);
    actions
}

fn handle(core: &mut Core, now: Instant, event: Event) -> Vec<Action> {
    let mut actions = Vec::new();
    core.handle(now, event, &mut actions);
    actions
}

fn drain(core: &mut Core, target: DrainTarget) -> Vec<Action> {
    let mut actions = Vec::new();
    core.begin_drain(secs(1), target, &mut actions);
    actions
}

/// The `OpenFile` of `f.txt` (in `/srv/b`) under `file`.
fn opens(actions: &[Action], file: FileId) -> bool {
    actions.contains(&Action::OpenFile {
        file,
        root: RootId(1),
        path: vec![b"f.txt".to_vec()],
    })
}

/// Answer `file`'s open: the 200 head is rendered.
fn opened(core: &mut Core, now: Instant, file: FileId) -> Vec<Action> {
    let meta = regular(SIZE);
    handle(core, now, Event::FileOpened { file, meta })
}

/// Send the head and the body of the response on `conn` to the end.
fn send_all(core: &mut Core, now: Instant, conn: ConnId, file: FileId) -> Vec<Action> {
    let stamp = stamp(SIZE);
    let mut actions = handle(core, now, Event::ChunkReady { file, stamp });
    let len = core.send_buffer(conn).len();
    actions.extend(handle(core, now, Event::Sent { conn, len }));
    let len = 9;
    actions.extend(handle(core, now, Event::FileSent { conn, len }));
    actions
}

fn file_head(persistence: Persistence) -> Vec<u8> {
    let mut head = Vec::new();
    render_file_head(TEXT, SIZE, persistence, None, &mut head);
    head
}

fn unavailable() -> Vec<u8> {
    let mut out = Vec::new();
    render_fixed(
        Status::ServiceUnavailable,
        Persistence::Close,
        BodyMode::Full,
        None,
        &mut out,
    );
    out
}

fn logged(actions: &[Action], status: Status) -> bool {
    actions.iter().any(|a| {
        matches!(a, Action::Log(Log::Response { status: s, request: Some(RequestSummary {
            method: Method::Get, .. }), .. }) if *s == status)
    })
}

fn closes(actions: &[Action], conn: ConnId) -> bool {
    actions.contains(&Action::Close { conn })
}

/// Two requests in one read; the drain begins while the first one's 200,
/// queued keep-alive, is still being sent. Returns the actions from the
/// end of that 200 on.
fn pipelined(core: &mut Core, target: DrainTarget) -> Vec<Action> {
    let conn = ConnId(1);
    accept(core, secs(0), 1, PEER);
    let two = [GET_F, GET_F].concat();
    assert!(opens(&deliver(core, secs(0), conn, &two), FileId(1)));
    assert!(logged(&opened(core, secs(0), FileId(1)), Status::Ok));
    assert_eq!(core.send_buffer(conn), file_head(Persistence::KeepAlive));
    assert!(drain(core, target).is_empty());
    send_all(core, secs(1), conn, FileId(1))
}

#[test]
fn restart_drain_serves_a_pipelined_request_with_close() {
    let mut core = files_core();
    let conn = ConnId(1);
    let actions = pipelined(&mut core, DrainTarget::Restart);
    assert!(opens(&actions, FileId(2)), "{actions:?}");
    assert!(!logged(&actions, Status::ServiceUnavailable), "{actions:?}");
    assert!(logged(&opened(&mut core, secs(1), FileId(2)), Status::Ok));
    assert_eq!(core.send_buffer(conn), file_head(Persistence::Close));
    let actions = send_all(&mut core, secs(1), conn, FileId(2));
    assert!(
        actions.contains(&Action::ShutdownWrite { conn }),
        "{actions:?}"
    );
}

#[test]
fn shutdown_drain_answers_a_pipelined_request_503() {
    let mut core = files_core();
    let actions = pipelined(&mut core, DrainTarget::Shutdown);
    assert!(!opens(&actions, FileId(2)), "{actions:?}");
    assert!(logged(&actions, Status::ServiceUnavailable), "{actions:?}");
    assert_eq!(core.send_buffer(ConnId(1)), unavailable());
}

/// Conn 1 is idle keep-alive after a 200, conn 2 accepted without a byte,
/// conn 3 idle too; then the drain. Returns its actions.
fn idle_then_drain(core: &mut Core, target: DrainTarget) -> Vec<Action> {
    for (id, file) in [(1, FileId(1)), (3, FileId(2))] {
        let conn = ConnId(id);
        accept(core, secs(0), id, PEER);
        assert!(opens(&deliver(core, secs(0), conn, GET_F), file));
        opened(core, secs(0), file);
        let actions = send_all(core, secs(0), conn, file);
        assert!(actions.contains(&Action::CloseFile { file }), "{actions:?}");
    }
    accept(core, secs(0), 2, PEER);
    drain(core, target)
}

#[test]
fn restart_drain_keeps_idle_connections() {
    let mut core = files_core();
    let actions = idle_then_drain(&mut core, DrainTarget::Restart);
    assert!(actions.is_empty(), "{actions:?}");
    assert!(!core.is_drained());
    // The next request on an idle connection is served, with close.
    let conn = ConnId(1);
    assert!(opens(&deliver(&mut core, secs(5), conn, GET_F), FileId(3)));
    assert!(logged(&opened(&mut core, secs(5), FileId(3)), Status::Ok));
    assert_eq!(core.send_buffer(conn), file_head(Persistence::Close));
    let actions = send_all(&mut core, secs(5), conn, FileId(3));
    assert!(
        actions.contains(&Action::ShutdownWrite { conn }),
        "{actions:?}"
    );
    // Conn 1 ends its lingering close (2 s); the fresh one ends at its
    // header deadline (10 s), silently as always; the other idle one at
    // its keep-alive deadline (60 s).
    let actions = handle(&mut core, secs(10), Event::TimerFired);
    assert_eq!(
        actions,
        [
            Action::Close { conn: ConnId(1) },
            Action::Close { conn: ConnId(2) }
        ]
    );
    let actions = handle(&mut core, secs(59), Event::TimerFired);
    assert!(!closes(&actions, ConnId(3)), "{actions:?}");
    let actions = handle(&mut core, secs(60), Event::TimerFired);
    assert_eq!(actions, [Action::Close { conn: ConnId(3) }]);
    assert!(core.is_drained());
}

#[test]
fn shutdown_drain_closes_idle_connections() {
    let mut core = files_core();
    let actions = idle_then_drain(&mut core, DrainTarget::Shutdown);
    assert_eq!(
        actions,
        [
            Action::Close { conn: ConnId(1) },
            Action::Close { conn: ConnId(2) },
            Action::Close { conn: ConnId(3) },
        ]
    );
    assert!(core.is_drained());
}

/// A connection delivered after the drain began sends a request, if it
/// is read at all (a refused one has no receive buffer).
fn accepted_after_drain(core: &mut Core, target: DrainTarget) -> Vec<Action> {
    assert!(drain(core, target).is_empty());
    let mut actions = accept(core, secs(1), 1, PEER);
    if !core.recv_buffer(ConnId(1)).is_empty() {
        actions.extend(deliver(core, secs(1), ConnId(1), GET_F));
    }
    actions
}

#[test]
fn restart_drain_serves_an_accepted_connection() {
    let mut core = files_core();
    let conn = ConnId(1);
    let actions = accepted_after_drain(&mut core, DrainTarget::Restart);
    assert!(opens(&actions, FileId(1)), "{actions:?}");
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Log(_))),
        "{actions:?}"
    );
    assert!(logged(&opened(&mut core, secs(1), FileId(1)), Status::Ok));
    assert_eq!(core.send_buffer(conn), file_head(Persistence::Close));
}

#[test]
fn shutdown_drain_refuses_an_accepted_connection() {
    let mut core = files_core();
    let conn = ConnId(1);
    let actions = accepted_after_drain(&mut core, DrainTarget::Shutdown);
    let rejected = Action::Log(Log::Rejected {
        conn,
        peer: PEER,
        reason: RejectReason::ShuttingDown,
    });
    assert_eq!(actions.first(), Some(&rejected), "{actions:?}");
    assert!(!opens(&actions, FileId(1)), "{actions:?}");
    assert_eq!(core.send_buffer(conn), unavailable());
}

/// Half a head before the drain, the rest after it.
fn partial_head(core: &mut Core, target: DrainTarget) -> Vec<Action> {
    let conn = ConnId(1);
    accept(core, secs(0), 1, PEER);
    let (first, rest) = GET_F.split_at(10);
    assert!(!opens(&deliver(core, secs(0), conn, first), FileId(1)));
    assert!(drain(core, target).is_empty());
    deliver(core, secs(1), conn, rest)
}

#[test]
fn restart_drain_answers_a_partial_head_once_complete() {
    let mut core = files_core();
    let conn = ConnId(1);
    let actions = partial_head(&mut core, DrainTarget::Restart);
    assert!(opens(&actions, FileId(1)), "{actions:?}");
    assert!(logged(&opened(&mut core, secs(1), FileId(1)), Status::Ok));
    assert_eq!(core.send_buffer(conn), file_head(Persistence::Close));
}

#[test]
fn shutdown_drain_answers_a_partial_head_503() {
    let mut core = files_core();
    let actions = partial_head(&mut core, DrainTarget::Shutdown);
    assert!(logged(&actions, Status::ServiceUnavailable), "{actions:?}");
    assert_eq!(core.send_buffer(ConnId(1)), unavailable());
}

/// A proxied request sent during the drain on a connection accepted
/// before it (its first byte read before, so a shutdown keeps it).
fn proxied(core: &mut Core, target: DrainTarget) -> Vec<Action> {
    accept(core, secs(0), 1, PEER);
    deliver(core, secs(0), ConnId(1), b"G");
    assert!(drain(core, target).is_empty());
    deliver(
        core,
        secs(1),
        ConnId(1),
        b"ET /p HTTP/1.1\r\nHost: b.test\r\n\r\n",
    )
}

#[test]
fn restart_drain_forwards_a_proxied_request() {
    let mut core = files_core();
    let actions = proxied(&mut core, DrainTarget::Restart);
    assert!(
        actions.iter().any(|a| matches!(a, Action::Connect { .. })),
        "{actions:?}"
    );
}

#[test]
fn shutdown_drain_answers_a_proxied_request_503() {
    let mut core = files_core();
    let actions = proxied(&mut core, DrainTarget::Shutdown);
    assert!(
        !actions.iter().any(|a| matches!(a, Action::Connect { .. })),
        "{actions:?}"
    );
}

#[test]
fn abort_after_a_restart_drain_refuses_stragglers() {
    let mut core = files_core();
    assert!(drain(&mut core, DrainTarget::Restart).is_empty());
    let mut actions = Vec::new();
    core.abort_all(&mut actions);
    let actions = accept(&mut core, secs(1), 1, PEER);
    assert_eq!(
        actions.first(),
        Some(&Action::Log(Log::Rejected {
            conn: ConnId(1),
            peer: PEER,
            reason: RejectReason::ShuttingDown,
        }))
    );
}

/// A shutdown's drain is final: a restart's `begin_drain` after it
/// changes nothing (driver mistake tolerated), so a straggler is still
/// refused and a request completed later still gets the 503.
#[test]
fn restart_after_a_shutdown_drain_is_ignored() {
    let mut core = files_core();
    accept(&mut core, secs(0), 1, PEER);
    deliver(&mut core, secs(0), ConnId(1), b"G");
    assert!(drain(&mut core, DrainTarget::Shutdown).is_empty());
    assert!(drain(&mut core, DrainTarget::Restart).is_empty());
    let actions = deliver(&mut core, secs(1), ConnId(1), &GET_F[1..]);
    assert!(logged(&actions, Status::ServiceUnavailable), "{actions:?}");
    let actions = accept(&mut core, secs(1), 2, PEER);
    assert_eq!(
        actions.first(),
        Some(&Action::Log(Log::Rejected {
            conn: ConnId(2),
            peer: PEER,
            reason: RejectReason::ShuttingDown,
        }))
    );
}

/// A second restart's drain changes nothing; a shutdown's after it
/// escalates: idle and fresh connections close at once, stragglers are
/// refused.
#[test]
fn a_shutdown_drain_escalates_a_restart_drain() {
    let mut core = files_core();
    let actions = idle_then_drain(&mut core, DrainTarget::Restart);
    assert!(actions.is_empty(), "{actions:?}");
    assert!(drain(&mut core, DrainTarget::Restart).is_empty());
    assert_eq!(
        drain(&mut core, DrainTarget::Shutdown),
        [
            Action::Close { conn: ConnId(1) },
            Action::Close { conn: ConnId(2) },
            Action::Close { conn: ConnId(3) },
        ]
    );
    assert!(core.is_drained());
    let actions = accepted_after_drain(&mut core, DrainTarget::Shutdown);
    assert!(
        matches!(
            actions.first(),
            Some(Action::Log(Log::Rejected {
                reason: RejectReason::ShuttingDown,
                ..
            }))
        ),
        "{actions:?}"
    );
}
