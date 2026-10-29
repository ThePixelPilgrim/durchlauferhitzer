//! The descriptor budget (NFR-RES-05), checked against `RLIMIT_NOFILE`
//! at startup (A11, after binding).
//!
//! Descriptors are numbered from 0, so the soft limit must be at least
//! the number the server can hold at once. That is what is open at the
//! check (owner decision 2026-10-28, O5: counted in `/proc/self/fd`, so
//! fds 0–2, the roots, the listeners and anything inherited are all
//! included) plus what the run adds:
//!
//! ```text
//! needed = open now               fds 0-2, roots, listeners, inherited
//!        + 3                      epoll, the wake eventfd, the signalfd
//!        + 2 x max_connections    client sockets, refused ones included
//!        + (roots > 0)    1 + max_connections + disk_threads
//!                                 the disk pool's wake duplicate, the
//!                                 files open or being opened (orphans
//!                                 included, FR-CONN-11), and the closed
//!                                 files whose readahead a worker still
//!                                 runs (OA1: a queued one is cancelled)
//!        + (backends > 0) max_connections
//!                                 upstream sockets, one per counted
//!                                 connection
//! ```
//!
//! A soft limit below it is raised to exactly it when the hard limit
//! allows; otherwise startup fails naming both numbers.

use super::Failure;
use crate::sys::rlimit::{nofile, raise_nofile};

/// What the budget is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Descriptors {
    /// Open at the check ([`open_now`]).
    pub(super) open: u64,
    pub(super) max_connections: u32,
    /// `limits.disk_threads`.
    pub(super) disk_threads: u32,
    pub(super) roots: usize,
    pub(super) backends: usize,
}

/// The descriptors open now: the entries of `/proc/self/fd`, less the one
/// reading it.
pub(super) fn open_now() -> Result<u64, Failure> {
    let entries = std::fs::read_dir("/proc/self/fd")
        .map_err(|err| {
            Failure(format!(
                "cannot count open descriptors (/proc/self/fd): {err}"
            ))
        })?
        .count();
    Ok(u64::try_from(entries).unwrap_or(u64::MAX).saturating_sub(1))
}

/// The number of descriptors the server can hold at once (the formula in
/// the module docs); saturating.
pub(super) fn needed(d: Descriptors) -> u64 {
    let conns = u64::from(d.max_connections);
    let mut total = d
        .open
        .saturating_add(3)
        .saturating_add(conns.saturating_mul(2));
    if d.roots > 0 {
        total = total
            .saturating_add(1)
            .saturating_add(conns)
            .saturating_add(u64::from(d.disk_threads));
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
    use super::{Descriptors, needed, open_now};

    fn d(max_connections: u32, roots: usize, backends: usize) -> Descriptors {
        Descriptors {
            open: 7,
            max_connections,
            disk_threads: 4,
            roots,
            backends,
        }
    }

    #[test]
    fn the_formula() {
        // 7 open + 3 + 2 x 100.
        assert_eq!(needed(d(100, 0, 0)), 210);
        // + the pool's duplicate + 100 files (the roots are among the open)
        // + 4 closed files whose readahead a worker still runs (OA1).
        assert_eq!(needed(d(100, 3, 0)), 210 + 1 + 100 + 4);
        let eight = Descriptors {
            disk_threads: 8,
            ..d(100, 3, 0)
        };
        assert_eq!(needed(eight), 210 + 1 + 100 + 8);
        // Without roots there is no pool: the thread count does not count.
        let no_roots = Descriptors {
            disk_threads: 8,
            ..d(100, 0, 0)
        };
        assert_eq!(needed(no_roots), 210);
        // + 100 upstream sockets, however many backends.
        assert_eq!(needed(d(100, 0, 1)), 310);
        assert_eq!(needed(d(100, 0, 5)), 310);
        assert_eq!(needed(d(100, 3, 5)), 210 + 1 + 100 + 4 + 100);
    }

    #[test]
    fn open_now_counts_a_new_descriptor() {
        // Other test threads may open or close meanwhile, so only a lower
        // bound holds: this file is open, and the reading one not counted.
        let file = std::fs::File::open("/proc/self/status").unwrap();
        assert!(open_now().unwrap() >= 1);
        drop(file);
    }
}
