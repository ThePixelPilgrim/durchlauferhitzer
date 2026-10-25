//! Structured log entries the core hands to the driver (`Action::Log`).
//!
//! The core decides *what* is logged; the driver decides how and where it is
//! written. Entries carry only data, never preformatted text, so a driver can
//! escape untrusted parts (host, path) for its output format.

use std::net::IpAddr;

use crate::core::ConnId;
use crate::http::request::{HttpError, Method};
use crate::http::status::Status;
use crate::routing::RouteMiss;

/// One log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Log {
    /// A response was queued for sending (emitted when it is queued, not when
    /// the client has received it; a later send timeout or reset is logged
    /// as its own entry). `request` is `None` when the request head could not
    /// be parsed far enough to summarise it.
    Response {
        conn: ConnId,
        peer: IpAddr,
        status: Status,
        request: Option<RequestSummary>,
    },
    /// A connection or request was refused without a normal response cycle.
    Rejected {
        conn: ConnId,
        peer: IpAddr,
        reason: RejectReason,
    },
    /// The connection failed at the transport level (reset by peer or another
    /// socket error reported by the driver).
    Reset { conn: ConnId, peer: IpAddr },
}

/// The parts of a request worth logging. `host` and `path` come from the
/// client and are untrusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSummary {
    pub method: Method,
    pub host: Option<String>,
    pub path: String,
}

/// Why something was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Http(HttpError),
    Route(RouteMiss),
    Timeout(TimeoutKind),
    Limit(LimitKind),
}

/// Which timeout fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// The whole request head did not arrive within `header_timeout`.
    Header,
    /// The client stopped reading the response for `send_idle_timeout`.
    Send,
}

/// Which connection limit was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Connections,
    ConnectionsPerIp,
}
