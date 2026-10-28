//! The descriptor budget (NFR-RES-05), checked against `RLIMIT_NOFILE`
//! at startup (A11, after binding).
//!
//! Descriptors are numbered from 0, so the soft limit must be at least
//! the number the server can hold at once:
//!
//! ```text
//! needed = 3                      fds 0-2 (always open, FR-PROC-04)
//!        + 3                      epoll, the wake eventfd, the signalfd
//!        + listeners + roots
//!        + 2 x max_connections    client sockets, refused ones included
//!        + (roots > 0)    1 + max_connections
//!                                 the disk pool's wake duplicate, and
//!                                 the files open or being opened
//!                                 (orphans included, FR-CONN-11)
//!        + (backends > 0) max_connections
//!                                 upstream sockets, one per counted
//!                                 connection
//! ```
//!
//! A soft limit below it is raised to it when the hard limit allows;
//! otherwise startup fails naming both numbers.

use super::Failure;
use crate::sys::rlimit::{nofile, raise_nofile};

/// What the budget is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Descriptors {
    pub(super) max_connections: u32,
    pub(super) listeners: usize,
    pub(super) roots: usize,
    pub(super) backends: usize,
}

/// The number of descriptors the server can hold at once (the formula in
/// the module docs); saturating.
pub(super) fn needed(d: Descriptors) -> u64 {
    let conns = u64::from(d.max_connections);
    let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let mut total = 3u64
        .saturating_add(3)
        .saturating_add(count(d.listeners))
        .saturating_add(count(d.roots))
        .saturating_add(conns.saturating_mul(2));
    if d.roots > 0 {
        total = total.saturating_add(1).saturating_add(conns);
    }
    if d.backends > 0 {
        total = total.saturating_add(conns);
    }
    total
}

/// Makes sure the soft `RLIMIT_NOFILE` is at least `needed`: raises it
/// when the hard limit allows, fails otherwise.
pub(super) fn ensure(needed: u64) -> Result<(), Failure> {
    let (soft, hard) =
        nofile().map_err(|err| Failure(format!("cannot read RLIMIT_NOFILE: {err}")))?;
    if soft >= needed {
        return Ok(());
    }
    if hard < needed {
        return Err(Failure(format!(
            "too few file descriptors (RLIMIT_NOFILE): the server needs {needed}, \
             {hard} are available"
        )));
    }
    raise_nofile(needed).map_err(|err| {
        Failure(format!(
            "too few file descriptors (RLIMIT_NOFILE): the server needs {needed}, \
             raising the soft limit from {soft} failed: {err}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::{Descriptors, needed};

    fn d(max_connections: u32, roots: usize, backends: usize) -> Descriptors {
        Descriptors {
            max_connections,
            listeners: 2,
            roots,
            backends,
        }
    }

    #[test]
    fn the_formula() {
        // 3 + 3 + 2 listeners + 2 x 100.
        assert_eq!(needed(d(100, 0, 0)), 208);
        // + 3 roots + the pool's duplicate + 100 files.
        assert_eq!(needed(d(100, 3, 0)), 208 + 3 + 1 + 100);
        // + 100 upstream sockets, however many backends.
        assert_eq!(needed(d(100, 0, 1)), 308);
        assert_eq!(needed(d(100, 0, 5)), 308);
        assert_eq!(needed(d(100, 3, 5)), 208 + 3 + 1 + 100 + 100);
        // The defaults with one root and a proxy route: 4103 + 1027.
        assert_eq!(needed(d(1024, 1, 1)), 3 + 3 + 2 + 1 + 1 + 4 * 1024);
    }
}
