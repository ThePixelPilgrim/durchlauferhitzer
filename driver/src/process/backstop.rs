//! The kernel memory backstop (NFR-RES-09): at startup, between the
//! descriptor budget and the privilege drop, the soft and the hard
//! `RLIMIT_DATA` are both set to [`limit`], so the process cannot raise
//! them again and an allocation past them fails (Rust's allocation
//! failure aborts the process, which the supervisor restarts).
//!
//! `RLIMIT_DATA` (Linux 4.7 and later) counts the private writable
//! mappings: the heap, `mmap`ed allocations and the threads' stacks, not
//! the `PROT_NONE` reservations of glibc's arenas and not the main
//! thread's stack. `RLIMIT_AS` would count those reservations, which
//! glibc makes per thread.
//!
//! ```text
//! limit = memory_limit                      NFR-RES-01's budget
//!       + BASE_ALLOWANCE                    what it leaves outside
//!       + disk_threads x (DISK_THREAD_STACK + DISK_THREAD_OVERHEAD)
//!       + max_connections x PER_CONNECTION_ALLOWANCE
//! ```
//!
//! saturating. An inherited hard limit below it fails startup (exit 2)
//! naming both numbers, like the descriptor budget (NFR-RES-05). An
//! allowance that is too small shows at startup, not under load: the
//! disk workers are spawned after the backstop is set, and with too
//! little room the spawn fails (`durchlauferhitzer: driver: Resource
//! temporarily unavailable (os error 11)`, exit 2; seen with 64 workers
//! under a limit 2 MiB short).
//!
//! # The measurement (2026-11-09, x86_64, 4 CPUs)
//!
//! `VmData` (`/proc/<pid>/status`) of the server with the backstop off,
//! run as `nobody`, with a static root and a proxy route to a local
//! backend, `max_connections 16` and `memory_limit` 1 068 608 (the
//! smallest FR-CFG-21 accepts: 16 x 66 788, the worst case per
//! connection); in bytes, glibc (the system's) / musl (Rust's bundled
//! musl 1.2, linked statically; Alpine's CI job runs the dynamic one):
//!
//! ```text
//!                                              glibc         musl
//! ready, 4 disk threads                    9 228 288    8 663 040
//! after the load, worst of 6 rounds       10 014 720    8 695 808
//!   (8 GETs of 1 MiB + 8 POSTs of 64 KiB, 16 + 0, 0 + 16)
//! ready, max_connections 4096              9 228 288    8 663 040
//! 4096, with 4080 idle connections open   84 557 824   95 010 816
//! ready, 64 disk threads                 139 591 680  135 516 160
//! ```
//!
//! - Per connection: startup preallocates nothing per connection (ready
//!   is the same at 16 and at 4096), so [`PER_CONNECTION_ALLOWANCE`]
//!   was measured with connections open: (84 557 824 - 9 228 288) /
//!   4080 = 18 463 bytes on glibc, (95 010 816 - 8 663 040) / 4080 =
//!   21 165 on musl; less the budgeted client buffers (16 384 receive +
//!   1 024 send = 17 408), 1 055 and 3 757 bytes of per-entry state and
//!   allocator rounding (musl's size classes). 3 757 rounded up to a KiB
//!   is 4 KiB, plus the 1 KiB send buffer of a refused connection
//!   (`SEND_CAPACITY`, allocated on demand, outside the budget): 5 KiB.
//! - Per disk thread: (139 591 680 - 9 228 288) / 60 = 2 172 723 bytes
//!   on glibc, the 2 MiB stack plus 75.6 KB; on musl 2 114 219, the stack
//!   plus 16.7 KB. `/proc/<pid>/maps` shows each thread's 12 KiB
//!   `sigaltstack` and glibc's 132 KiB arenas (31 for 64 threads here;
//!   glibc makes up to 8 x CPUs, so one per thread on 8 or more CPUs).
//!   [`DISK_THREAD_OVERHEAD`] is 256 KiB: one arena and the sigaltstack,
//!   144 KiB, with margin.
//! - Base: the loaded glibc reading less what the other terms count,
//!   10 014 720 - 1 068 608 - 4 x 2 MiB - 16 x 5 KiB = 475 584 (musl's
//!   is lower), rounded up to a MiB and doubled: 2 MiB. Doubled again to
//!   [`BASE_ALLOWANCE`], 4 MiB: allocator bookkeeping and glibc's
//!   main-arena top pad grow with none of the other terms, and 2 MiB of
//!   slack costs nothing, while 2 MiB too little aborts a server.

use durchlauferhitzer_core::config::Limits;

use super::Failure;
use crate::log::DriverLog;
use crate::sys::rlimit::{cap_data, data};

pub use crate::pool::DISK_THREAD_STACK;

/// What `memory_limit` leaves outside and grows with neither the
/// connections nor the disk workers (measured, module docs).
pub const BASE_ALLOWANCE: u64 = 4 * 1024 * 1024;

/// What each disk worker maps besides its [`DISK_THREAD_STACK`]: its
/// `sigaltstack` and, on glibc, an arena (measured, module docs).
pub const DISK_THREAD_OVERHEAD: u64 = 256 * 1024;

/// What each counted connection holds outside `memory_limit`: its
/// per-entry state and allocator rounding, and a refused connection's
/// send buffer (measured, module docs).
pub const PER_CONNECTION_ALLOWANCE: u64 = 5 * 1024;

/// The `RLIMIT_DATA` the backstop sets for `limits` (the formula in the
/// module docs); saturating.
pub fn limit(limits: &Limits) -> u64 {
    let stack = u64::try_from(DISK_THREAD_STACK).unwrap_or(u64::MAX);
    limits
        .memory_limit
        .saturating_add(BASE_ALLOWANCE)
        .saturating_add(
            u64::from(limits.disk_threads)
                .saturating_mul(stack.saturating_add(DISK_THREAD_OVERHEAD)),
        )
        .saturating_add(u64::from(limits.max_connections).saturating_mul(PER_CONNECTION_ALLOWANCE))
}

/// Sets the soft and hard `RLIMIT_DATA` to [`limit`]; a hard limit
/// below it fails naming both numbers. The note is the startup log's
/// `memory_backstop` line.
pub(super) fn apply(limits: &Limits) -> Result<DriverLog, Failure> {
    let needed = limit(limits);
    let (_, hard) = data().map_err(|err| Failure(format!("cannot read RLIMIT_DATA: {err}")))?;
    if hard < needed {
        return Err(Failure(format!(
            "memory backstop: the server needs {needed} bytes of RLIMIT_DATA, \
             {hard} are available"
        )));
    }
    cap_data(needed).map_err(|err| {
        Failure(format!(
            "memory backstop: setting RLIMIT_DATA to {needed} bytes failed: {err}"
        ))
    })?;
    Ok(DriverLog::MemoryBackstop { bytes: needed })
}

#[cfg(test)]
mod tests {
    use durchlauferhitzer_core::config::Limits;

    use super::{
        BASE_ALLOWANCE, DISK_THREAD_OVERHEAD, DISK_THREAD_STACK, PER_CONNECTION_ALLOWANCE, limit,
    };

    #[test]
    fn limit_is_memory_limit_plus_the_three_allowance_terms() {
        let limits = Limits {
            memory_limit: 1 << 20,
            disk_threads: 4,
            max_connections: 16,
            ..Limits::default()
        };
        assert_eq!(DISK_THREAD_STACK, 2 << 20);
        assert_eq!(
            limit(&limits),
            (1 << 20)
                + BASE_ALLOWANCE
                + 4 * ((2 << 20) + DISK_THREAD_OVERHEAD)
                + 16 * PER_CONNECTION_ALLOWANCE
        );
    }

    #[test]
    fn limit_saturates() {
        let limits = Limits {
            memory_limit: u64::MAX,
            ..Limits::default()
        };
        assert_eq!(limit(&limits), u64::MAX);
    }
}
