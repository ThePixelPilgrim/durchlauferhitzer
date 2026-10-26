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
