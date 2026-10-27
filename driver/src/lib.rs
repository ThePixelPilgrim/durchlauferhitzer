//! The Linux epoll driver of durchlauferhitzer (stage 4): it owns every
//! socket, file, thread and signal, and drives the sans-IO core
//! (`durchlauferhitzer-core`) through its driver contract.
//!
//! All `unsafe` code lives in [`sys`]; the rest of the crate is safe Rust.

#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

#[cfg(not(target_os = "linux"))]
compile_error!("durchlauferhitzer runs on Linux only (NFR-PORT-01: epoll, openat2, signalfd)");

#[cfg(not(target_pointer_width = "64"))]
compile_error!(
    "durchlauferhitzer needs a 64-bit target (NFR-PORT-01: x86_64, aarch64; 64-bit off_t)"
);

#[allow(unsafe_code)]
pub mod sys;
