//! Helpers shared by the core's unit tests.

use std::net::{IpAddr, Ipv4Addr};

use crate::core::{Action, ConnId, Core, Event};
use crate::file::{FileKind, FileMeta, FileStamp};
use crate::time::Instant;

pub(super) const CONFIG: &str = "listen 127.0.0.1:8080; \
    limits { max_request_line 8; max_head 8; } \
    host a.test { path / { static /srv; } }";

pub(super) fn core() -> Core {
    Core::new(crate::config::load(CONFIG).unwrap())
}

pub(super) const PEER: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

pub(super) fn secs(n: u64) -> Instant {
    Instant::from_nanos(0).saturating_add(std::time::Duration::from_secs(n))
}

pub(super) fn accept(core: &mut Core, now: Instant, id: u64, peer: IpAddr) -> Vec<Action> {
    let mut actions = Vec::new();
    let conn = ConnId(id);
    core.handle(now, Event::Accepted { conn, peer }, &mut actions);
    actions
}

pub(super) const FILES: &str = "listen 127.0.0.1:8080; \
    host a.test { path / { static /srv/a; } path /x { static /srv/b; } } \
    host b.test { path / { static /srv/a; } path /p { proxy 127.0.0.1:3000; \
    max_request_body 1MiB; max_response_body unlimited; } path /c { static /srv/c; } }";

pub(super) fn files_core() -> Core {
    Core::new(crate::config::load(FILES).unwrap())
}

/// Accept `id` and deliver `request` in one read; returns the actions.
pub(super) fn request(core: &mut Core, now: Instant, id: u64, request: &[u8]) -> Vec<Action> {
    let mut actions = accept(core, now, id, PEER);
    let conn = ConnId(id);
    let spare = core.recv_buffer(conn);
    spare
        .get_mut(..request.len())
        .unwrap()
        .copy_from_slice(request);
    let len = request.len();
    actions.clear();
    core.handle(now, Event::Received { conn, len }, &mut actions);
    actions
}

pub(super) const GET_F: &[u8] = b"GET /x/f.txt HTTP/1.1\r\nHost: a.test\r\n\r\n";

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
