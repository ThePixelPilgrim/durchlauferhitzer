//! Thin, checked wrappers around the Linux system calls the driver uses.
//!
//! This is the only module of the crate that may contain `unsafe` code.
//! Every wrapper is safe to call: it takes borrowed or owned descriptors
//! (`BorrowedFd`, `OwnedFd`) and Rust buffers, checks the `-1`/errno
//! return and reports it as an `io::Error` (an [`fd::Io`] for transfers),
//! and every `unsafe` block states why it is sound (`// SAFETY:`).

pub mod epoll;
pub mod eventfd;
pub mod fd;
pub mod file;
pub mod rlimit;
pub mod signal;
pub mod socket;
pub mod user;
