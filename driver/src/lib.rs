//! The Linux epoll driver of durchlauferhitzer (stage 4): it owns every
//! socket, file, thread and signal, and drives the sans-IO core
//! (`durchlauferhitzer-core`) through its driver contract.
//!
//! All `unsafe` code lives in [`sys`]; the rest of the crate is safe Rust.

#![deny(unsafe_code)]

#[cfg(not(target_os = "linux"))]
compile_error!("durchlauferhitzer runs on Linux only (NFR-PORT-01: epoll, openat2, signalfd)");

#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "durchlauferhitzer needs a 64-bit target (NFR-PORT-01: x86_64, aarch64; 64-bit off_t)"
);

mod conns;
pub mod event_loop;
pub mod log;
pub mod pool;
#[allow(unsafe_code)]
pub mod sys;

pub use event_loop::{Driver, Exit, StopHandle};
pub use log::{DriverLog, LogEntry, LogSink};
pub use pool::Pool;
