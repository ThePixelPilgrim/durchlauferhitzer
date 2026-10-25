//! Unit tests of the core, by area.

mod connections;
mod static_files;

use super::*;
use crate::file::{FileKind, FileMeta, FileStamp};
use crate::http::response::render_file_head;
use std::net::Ipv4Addr;

const PEER: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

fn secs(n: u64) -> Instant {
    Instant::from_nanos(0).saturating_add(std::time::Duration::from_secs(n))
}

fn accept(core: &mut Core, now: Instant, id: u64, peer: IpAddr) -> Vec<Action> {
    let mut actions = Vec::new();
    let conn = ConnId(id);
    core.handle(now, Event::Accepted { conn, peer }, &mut actions);
    actions
}
