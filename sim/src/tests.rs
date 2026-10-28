//! Unit tests of the sim's own invariant checks.

use super::*;
use crate::disk::Phase;
use durchlauferhitzer_core::{BackendId, FileStamp, RootId};
use std::net::Ipv4Addr;

const CONFIG: &str = "listen 127.0.0.1:8080; host a.test { path / { static /srv; } }";

#[test]
#[should_panic(expected = "read into recv_buffer of conn=1 not reported")]
fn unreported_read_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    // Simulate a driver bug: bytes written into conn 1's receive buffer,
    // then a different event handled before the matching `Received`.
    sim.pending_read = Some(Pending::Conn(c));
    sim.deliver(Event::TimerFired);
}

#[test]
#[should_panic(
    expected = "conn=1 wants to write but send_buffer is empty and send_file has no segment"
)]
fn write_interest_with_nothing_to_send_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    // Simulate a core bug: interest `Write` while neither the send buffer
    // nor `send_file` has a byte (a fresh connection only wants to read).
    sim.conn_mut(c).interest = Interest::Write;
    sim.check_buffers();
}

fn track(sim: &mut Sim, conn: ConnId, file: u64, phase: Phase) {
    sim.files.insert(
        FileId(file),
        FileTrack {
            conn,
            root: "/srv".to_owned(),
            key: Vec::new(),
            phase,
        },
    );
}

fn open(size: u64) -> Phase {
    Phase::Open {
        size,
        chunks: 0,
        ready: Vec::new(),
        handle: None,
    }
}

#[test]
#[should_panic(expected = "second CloseFile for file=1")]
fn second_close_file_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    track(&mut sim, c, 1, open(5));
    sim.apply(Action::CloseFile { file: FileId(1) });
    sim.apply(Action::CloseFile { file: FileId(1) });
}

#[test]
#[should_panic(expected = "Readahead for unknown or closed file=9")]
fn action_for_unknown_file_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    sim.apply(Action::Readahead {
        file: FileId(9),
        offset: 0,
        len: 1,
    });
}

#[test]
#[should_panic(expected = "beyond the size 5 seen at open")]
fn readahead_beyond_size_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    track(&mut sim, c, 1, open(5));
    sim.apply(Action::Readahead {
        file: FileId(1),
        offset: 2,
        len: 4,
    });
}

#[test]
#[should_panic(expected = "file=1 left open after conn=1 closed")]
fn file_left_open_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    track(&mut sim, c, 1, open(5));
    sim.conn_mut(c).state = SimConnState::Closed;
    sim.check_files();
}

#[test]
#[should_panic(expected = "send_file segment 0..5 of file=1 before its ChunkReady")]
fn segment_before_chunk_ready_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    sim.add_file("/srv", "f", b"hello");
    sim.set_disk_delay(Duration::from_secs(1));
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    sim.send(c, b"GET /f HTTP/1.1\r\nHost: a.test\r\n\r\n");
    sim.run_until_idle();
    // Open completes, then the head is written; the readahead is still
    // in flight when the core is handed its ChunkReady behind the sim's
    // back, so the sim never saw that range become ready.
    sim.advance(Duration::from_secs(1));
    sim.run_until_idle();
    let stamp = FileStamp {
        size: 5,
        modified_ns: 1,
    };
    let mut actions = Vec::new();
    let now = sim.now;
    sim.core.handle(
        now,
        Event::ChunkReady {
            file: FileId(1),
            stamp,
        },
        &mut actions,
    );
    sim.check_files();
}

#[test]
#[should_panic(expected = "OpenFile file=1 outside a connection's event")]
fn open_file_outside_a_connection_event_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    sim.apply(Action::OpenFile {
        file: FileId(1),
        root: RootId(0),
        path: Vec::new(),
    });
}

#[test]
fn disk_queue_is_ordered_by_due_time() {
    let mut sim = Sim::new(CONFIG);
    let at = |n| Instant::from_nanos(n);
    for (due, file) in [(5, 1), (3, 2), (5, 3), (1, 4), (3, 5)] {
        sim.queue_disk(at(due), DiskOp::Open { file: FileId(file) });
    }
    let order: Vec<(u64, u64)> = sim
        .disk
        .iter()
        .map(|(due, op)| (due.as_nanos(), op.file().0))
        .collect();
    assert_eq!(order, [(1, 4), (3, 2), (3, 5), (5, 1), (5, 3)]);
}

#[test]
#[should_panic(expected = "run_until_idle exceeded 5 steps")]
fn runaway_pump_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    sim.step_limit = 5;
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    sim.set_read_chunk(c, 1);
    sim.send(c, b"0123456789");
    sim.run_until_idle();
}

const PROXY: &str = "listen 127.0.0.1:8080; host a.test { path / { proxy 127.0.0.1:3000; \
    max_request_body 1MiB; max_response_body 1MiB; } }";

/// A sim with conn 1 open and upstream 1 connecting for it.
fn connecting() -> (Sim, ConnId, UpstreamId) {
    let mut sim = Sim::new(PROXY);
    sim.set_connect("127.0.0.1:3000", ConnectBehaviour::Hang);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    sim.send(c, b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n");
    sim.run_until_idle();
    (sim, c, UpstreamId(1))
}

#[test]
fn upstream_lifecycle_is_tracked() {
    let (mut sim, _, up) = connecting();
    assert_eq!(sim.upstreams(), [up]);
    assert_eq!(sim.open_upstreams(), 1);
    sim.apply(Action::CloseUpstream { up });
    assert_eq!(sim.open_upstreams(), 0);
}

#[test]
#[should_panic(expected = "second CloseUpstream for up=1")]
fn second_close_upstream_is_a_violation() {
    let (mut sim, _, up) = connecting();
    sim.apply(Action::CloseUpstream { up });
    sim.apply(Action::CloseUpstream { up });
}

#[test]
#[should_panic(expected = "SetUpstreamInterest for unknown or closed up=9")]
fn action_for_unknown_upstream_is_a_violation() {
    let mut sim = Sim::new(PROXY);
    sim.apply(Action::SetUpstreamInterest {
        up: UpstreamId(9),
        interest: Interest::Read,
    });
}

#[test]
#[should_panic(expected = "SetUpstreamInterest for unknown or closed up=1")]
fn action_for_closed_upstream_is_a_violation() {
    let (mut sim, _, up) = connecting();
    sim.apply(Action::CloseUpstream { up });
    sim.apply(Action::SetUpstreamInterest {
        up,
        interest: Interest::Read,
    });
}

#[test]
#[should_panic(expected = "Connect up=1 outside a connection's event")]
fn connect_outside_a_connection_event_is_a_violation() {
    let mut sim = Sim::new(PROXY);
    sim.apply(Action::Connect {
        up: UpstreamId(1),
        backend: BackendId(0),
    });
}

#[test]
#[should_panic(expected = "Connect up=3 but the next id is 2")]
fn connect_with_a_skipped_id_is_a_violation() {
    let (mut sim, c, _) = connecting();
    sim.context = Some(c);
    sim.apply(Action::Connect {
        up: UpstreamId(3),
        backend: BackendId(0),
    });
}

#[test]
#[should_panic(expected = "Connect up=2 while conn=1 still has up=1 open")]
fn second_upstream_per_client_is_a_violation() {
    let (mut sim, c, _) = connecting();
    sim.context = Some(c);
    sim.apply(Action::Connect {
        up: UpstreamId(2),
        backend: BackendId(0),
    });
}

#[test]
#[should_panic(expected = "Connect up=2 for unknown backend=5")]
fn connect_to_unknown_backend_is_a_violation() {
    let (mut sim, c, up) = connecting();
    sim.apply(Action::CloseUpstream { up });
    sim.context = Some(c);
    sim.apply(Action::Connect {
        up: UpstreamId(2),
        backend: BackendId(5),
    });
}

#[test]
#[should_panic(expected = "up=1 left open after conn=1 closed")]
fn upstream_left_open_is_a_violation() {
    let (mut sim, c, _) = connecting();
    sim.conn_mut(c).state = SimConnState::Closed;
    sim.check_upstreams();
}

#[test]
#[should_panic(expected = "read into upstream_recv_buffer of up=1 not reported")]
fn unreported_upstream_read_is_a_violation() {
    let (mut sim, _, up) = connecting();
    sim.pending_read = Some(Pending::Upstream(up));
    sim.deliver(Event::TimerFired);
}

/// What the core offers for an upstream in a body: `recv` bytes to read
/// into, with `pending` body bytes unsent and a chunked tail of `tail`.
fn body_view(recv: usize, pending: usize) -> crate::backend::UpstreamView {
    tail_view(recv, pending, 0)
}

fn tail_view(recv: usize, unsent: usize, tail: usize) -> crate::backend::UpstreamView {
    crate::backend::UpstreamView {
        interest: if recv > 0 {
            Interest::Read
        } else {
            Interest::None
        },
        recv,
        send: 0,
        body_pending: Some(BodyPending { unsent, tail }),
    }
}

#[test]
fn a_full_read_into_a_drained_body_buffer_is_fine() {
    let (sim, _, up) = connecting();
    let cap = sim.backends.recv_bound;
    sim.check_upstream_view(up, &body_view(cap, 0));
    // Behind a chunked tail of up to 35 bytes, the rest of the buffer.
    sim.check_upstream_view(up, &tail_view(cap - 35, 0, 35));
    // Nothing offered while bytes (and a tail behind them) are unsent.
    sim.check_upstream_view(up, &tail_view(0, 9, 3));
}

#[test]
#[should_panic(expected = "up=1 keeps a chunked tail of 36 bytes, more than 35")]
fn long_response_chunked_tail_is_a_violation() {
    let (sim, _, up) = connecting();
    sim.check_upstream_view(up, &tail_view(0, 0, 36));
}

#[test]
#[should_panic(expected = "offers a body read of 16380 bytes behind a 3-byte tail, not the rest")]
fn short_read_behind_a_response_tail_is_a_violation() {
    let (sim, _, up) = connecting();
    sim.check_upstream_view(up, &tail_view(16380, 0, 3));
}

#[test]
#[should_panic(expected = "up=1 offers a read while 5 body bytes are unsent")]
fn read_offered_with_unsent_body_bytes_is_a_violation() {
    let (sim, _, up) = connecting();
    let cap = sim.backends.recv_bound;
    sim.check_upstream_view(up, &body_view(cap, 5));
}

#[test]
#[should_panic(expected = "offers a body read of 16383 bytes behind a 0-byte tail, not the rest")]
fn partial_body_read_is_a_violation() {
    let (sim, _, up) = connecting();
    sim.check_upstream_view(up, &body_view(16383, 0));
}

#[test]
#[should_panic(expected = "up=1 upstream_recv_buffer 16385 bytes exceeds capacity 16384")]
fn upstream_recv_buffer_over_capacity_is_a_violation() {
    let (sim, _, up) = connecting();
    let mut view = body_view(16385, 0);
    view.body_pending = None;
    sim.check_upstream_view(up, &view);
}

#[test]
#[should_panic(expected = "upstream_send_buffer 99999 bytes exceeds capacity")]
fn upstream_send_buffer_over_capacity_is_a_violation() {
    let (sim, _, up) = connecting();
    let mut view = body_view(0, 0);
    view.send = 99999;
    sim.check_upstream_view(up, &view);
}

#[test]
#[should_panic(expected = "up=1 wants to read but upstream_recv_buffer is empty")]
fn upstream_read_interest_with_empty_buffer_is_a_violation() {
    let (mut sim, _, up) = connecting();
    // The core offers nothing while the connect is pending.
    sim.apply(Action::SetUpstreamInterest {
        up,
        interest: Interest::Read,
    });
    sim.check_upstreams();
}

#[test]
#[should_panic(expected = "up=1 failed but the core never closed it")]
fn failed_upstream_left_open_is_a_violation() {
    let (mut sim, _, up) = connecting();
    sim.fake_upstream_end(up, true);
    sim.run_until_idle();
}

#[test]
#[should_panic(expected = "up=1 reached end of stream but the core never closed it")]
fn ended_upstream_left_open_is_a_violation() {
    let (mut sim, _, up) = connecting();
    sim.fake_upstream_end(up, false);
    sim.run_until_idle();
}

#[test]
#[should_panic(expected = "conn=1 send capacity 1024 is not one it may have (accepted with 7)")]
fn changed_send_capacity_is_a_violation() {
    let mut sim = Sim::new(CONFIG);
    let c = sim.connect(IpAddr::V4(Ipv4Addr::LOCALHOST));
    sim.conn_mut(c).send_capacity = 7;
    sim.check_buffers();
}

#[test]
fn a_body_read_behind_a_short_tail_is_fine() {
    let (sim, c, _) = connecting();
    let pending = |unsent, tail| Some(BodyPending { unsent, tail });
    sim.check_request_body(c, 100, 100, pending(0, 0));
    sim.check_request_body(c, 65, 100, pending(0, 35));
    // Nothing offered while bytes are unsent.
    sim.check_request_body(c, 0, 100, pending(7, 0));
    sim.check_request_body(c, 40, 100, None);
}

#[test]
#[should_panic(expected = "conn=1 offers a read while 7 request body bytes are unsent")]
fn request_body_read_with_unsent_bytes_is_a_violation() {
    let (sim, c, _) = connecting();
    sim.check_request_body(c, 93, 100, Some(BodyPending { unsent: 7, tail: 0 }));
}

#[test]
#[should_panic(expected = "offers a body read of 90 bytes behind a 2-byte tail, not the rest")]
fn partial_request_body_read_is_a_violation() {
    let (sim, c, _) = connecting();
    sim.check_request_body(c, 90, 100, Some(BodyPending { unsent: 0, tail: 2 }));
}

#[test]
#[should_panic(expected = "keeps a chunked tail of 36 bytes, more than 35")]
fn long_chunked_tail_is_a_violation() {
    let (sim, c, _) = connecting();
    sim.check_request_body(
        c,
        0,
        100,
        Some(BodyPending {
            unsent: 0,
            tail: 36,
        }),
    );
}

/// What a backend receives is judged as exactly one request, however the
/// bytes are split.
#[test]
fn forwarded_request_is_judged_as_one_message() {
    use crate::backend::Forwarded;
    let sim = Sim::new(PROXY);
    let limits = sim.backends.forwarded_limits;
    let judge = |pieces: &[&[u8]]| {
        let mut f = Forwarded::new();
        pieces
            .iter()
            .try_for_each(|p| f.feed(p, &limits))
            .map(|()| f)
    };
    let head = b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\n\r\n";
    assert_eq!(judge(&[head, b"ab", b"c"]), Ok(Forwarded::Done));
    assert_eq!(
        judge(&[head, b"abcGET"]),
        Err("3 bytes after the end of its Content-Length body".to_owned())
    );
    assert_eq!(
        judge(&[head, b"abc", b"x"]),
        Err("1 bytes after the end of its request".to_owned())
    );
    let chunked = b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n3\r";
    assert_eq!(
        judge(&[chunked, b"\nabc\r\n0\r\n", b"\r\n"]),
        Ok(Forwarded::Done)
    );
    assert_eq!(
        judge(&[chunked, b"\nabc\r\n0\r\n\r\nX"]),
        Err("1 bytes after the end of its chunked body".to_owned())
    );
    assert!(judge(&[chunked, b";x"]).is_err());
    assert!(judge(&[b"GET / HTTP/1.1\r\n\r\n"]).is_err());
    assert_eq!(
        judge(&[b"GET / HTTP/1.1\r\nHost: a\r\n\r\n", b"G"]),
        Err("1 bytes after the end of its request".to_owned())
    );
}

#[test]
#[should_panic(expected = "SetInterest ReadWrite for conn=1: it is for upstreams only")]
fn client_read_write_interest_is_a_violation() {
    let (mut sim, c, _) = connecting();
    sim.apply(Action::SetInterest {
        conn: c,
        interest: Interest::ReadWrite,
    });
}

#[test]
#[should_panic(expected = "up=1 wants to write but upstream_send_buffer is empty")]
fn upstream_read_write_with_nothing_to_write_is_a_violation() {
    let (sim, _, up) = connecting();
    let view = crate::backend::UpstreamView {
        interest: Interest::ReadWrite,
        recv: 100,
        send: 0,
        body_pending: None,
    };
    sim.check_upstream_view(up, &view);
}
