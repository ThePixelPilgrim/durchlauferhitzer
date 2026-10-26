//! Structured log entries the core hands to the driver (`Action::Log`).
//!
//! The core decides *what* is logged; the driver decides how and where it is
//! written. Entries carry only data, never preformatted text, so a driver can
//! escape untrusted parts (host, path) for its output format.

use std::net::IpAddr;

use crate::core::ConnId;
use crate::file::FileError;
use crate::http::chunked::ChunkedError;
use crate::http::request::{HttpError, Method};
use crate::http::response_head::ResponseError;
use crate::http::status::Status;
use crate::routing::RouteMiss;
use crate::upstream::BackendId;

/// One log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Log {
    /// A response of the core's own was queued for sending (emitted when it
    /// is queued, not when the client has received it; a later send timeout
    /// or reset is logged as its own entry). `request` is `None` when the
    /// request head could not be parsed far enough to summarise it. A 502 or
    /// 504 the core answers for a failed backend is one of these, after its
    /// [`Log::BackendFailed`].
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
    /// A file response was cut off after its head was queued: the file
    /// failed or changed mid-transfer, so the connection was closed and the
    /// client saw an incomplete body.
    FileAborted {
        conn: ConnId,
        peer: IpAddr,
        error: FileError,
    },
    /// A backend's response head was queued for the client (the proxied
    /// counterpart of [`Log::Response`], emitted at the same point). The
    /// status is the backend's, any code 100–599, so it is a plain number
    /// rather than one of the core's own [`Status`] values. A later failure
    /// (a cut body) is logged as its own entry.
    ProxiedResponse {
        conn: ConnId,
        peer: IpAddr,
        backend: BackendId,
        status: u16,
        request: RequestSummary,
    },
    /// Talking to `backend` failed for the request on `conn`. Before the
    /// response head was queued it is followed by the [`Log::Response`] of
    /// the 502 or 504 the client gets; after it, the connection is cut.
    BackendFailed {
        conn: ConnId,
        peer: IpAddr,
        backend: BackendId,
        kind: BackendFailure,
    },
}

/// How talking to a backend failed (FR-PROXY-31).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendFailure {
    /// The connect failed (refused, unreachable, or any other error before
    /// `UpstreamConnected`).
    Refused,
    /// The connected socket failed, or the backend closed it before its
    /// response was complete.
    Reset,
    /// The response head was malformed or oversized.
    BadResponse(ResponseError),
    /// The connect did not complete within the connect timeout.
    ConnectTimeout,
    /// The backend did not send its response head (or, after it, made no
    /// body progress) in time.
    ResponseTimeout,
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
    /// The request body on a proxy route broke a rule: answered 400 while
    /// its head had not been forwarded yet, else both connections were cut.
    Body(RequestBodyError),
}

/// What was wrong with a request body on a proxy route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestBodyError {
    /// Its chunked framing is invalid (FR-PROXY-19).
    Chunked(ChunkedError),
    /// Its chunked payload passed the route's `max_request_body`
    /// (FR-PROXY-23).
    TooLarge,
}

/// Which timeout fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// The whole request head did not arrive within `header_timeout`.
    Header,
    /// The client stopped reading the response for `send_idle_timeout`.
    Send,
    /// The disk did not complete an open or readahead within
    /// `send_idle_timeout`.
    Disk,
    /// The client made no progress with its request body on a proxy route
    /// for the body idle timeout, before the response head (FR-PROXY-26).
    RequestBody,
}

/// Which connection limit was hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Connections,
    ConnectionsPerIp,
}
