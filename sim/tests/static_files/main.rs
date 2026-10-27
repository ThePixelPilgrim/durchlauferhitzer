//! Static files through the simulation driver: the file interface between
//! core and driver, with the simulated file system behind it.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use durchlauferhitzer_core::http::request::{HttpError, Method, Persistence};
use durchlauferhitzer_core::http::response::BodyMode;
use durchlauferhitzer_core::http::status::Status;
use durchlauferhitzer_core::http::target::TargetError;
use durchlauferhitzer_core::{
    BackendFailure, BackendId, ConnId, FileError, FileStamp, Interest, LimitKind, Log,
    RejectReason, RequestSummary, TimeoutKind,
};
use durchlauferhitzer_sim::{ConnectBehaviour, Sim, SimConnState};

const CONFIG: &str = "listen 127.0.0.1:8080;
host example.test {
  path / { static /srv/www; }
  path /assets { static /srv/assets; }
}";

fn peer() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))
}

#[allow(dead_code, reason = "shared by all test binaries; each uses part")]
#[path = "../support/model_http.rs"]
mod model_http;

/// A file head, by the spec-written model, not the core's renderer.
fn head(content_type: &str, len: u64, persistence: Persistence) -> Vec<u8> {
    model_http::model_file_head(content_type, len, persistence)
}

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: example.test\r\n\r\n").into_bytes()
}

mod edge;
mod rules;
mod transfer;

use rules::*;
use transfer::*;
