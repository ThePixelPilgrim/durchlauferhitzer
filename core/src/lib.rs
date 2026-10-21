//! Core of durchlauferhitzer: pure, I/O-free logic.

pub mod config;
mod conn;
mod core;
pub mod http;
mod log;
pub mod routing;
mod time;

pub use crate::core::{Action, ConnError, ConnId, Core, Event, Interest};
pub use crate::log::{LimitKind, Log, RejectReason, RequestSummary, TimeoutKind};
pub use crate::time::Instant;
