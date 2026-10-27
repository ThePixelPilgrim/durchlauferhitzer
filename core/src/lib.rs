//! Core of durchlauferhitzer: pure, I/O-free logic.

#![forbid(unsafe_code)]

pub mod config;
mod conn;
mod core;
mod file;
pub mod http;
mod log;
mod relay_body;
mod request_body;
pub mod routing;
mod time;
mod upstream;

pub use crate::core::{Action, BodyPending, ConnError, ConnId, Core, Event, Interest};
pub use crate::file::{FileError, FileId, FileKind, FileMeta, FileSegment, FileStamp, RootId};
pub use crate::log::{
    BackendFailure, LimitKind, Log, RejectReason, RequestBodyError, RequestLine, RequestSummary,
    TimeoutKind,
};
pub use crate::time::Instant;
pub use crate::upstream::{BackendId, UpstreamId};
