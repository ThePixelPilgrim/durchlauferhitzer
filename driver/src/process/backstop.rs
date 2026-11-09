//! The kernel memory backstop (NFR-RES-09): at startup, between the
//! descriptor budget and the privilege drop, the soft and the hard
//! `RLIMIT_DATA` are both set to [`limit`], so the process cannot raise
//! them again and an allocation past them fails (Rust's allocation
//! failure aborts the process, which the supervisor restarts; by SIGABRT,
//! or by SIGSYS with `RUST_BACKTRACE` set under `seccomp_action kill`,
//! since the backtrace's `getcwd` is not allowed). The backstop bounds
//! the process's own memory: kernel memory (socket buffers, epoll state,
//! the page cache) is outside `RLIMIT_DATA`.
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
//!       + max_connections x (PER_CONNECTION_FIXED
//!                          + REQUEST_STATE_FACTOR x max_head)
//! ```
//!
//! saturating. An inherited hard limit below it fails startup (exit 2)
//! naming both numbers, like the descriptor budget (NFR-RES-05). A
//! thread term that is too small shows at startup: the disk workers are
//! spawned after the backstop is set, and with too little room the spawn
//! fails (`durchlauferhitzer: driver: cannot start disk thread <n> of
//! <threads>: Resource temporarily unavailable (os error 11);
//! RLIMIT_DATA is the memory backstop (<bytes> bytes, NFR-RES-09)`,
//! exit 2; seen with 64 workers under a limit 2 MiB short). The per-connection term is what load needs, so it carries the
//! worst case of every structure a connection keeps outside the budget,
//! its request in flight included: too small, it aborts a loaded server
//! (the stage 9 review's C1, when it was a constant measured idle).
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
//! - Per connection, idle: startup preallocates nothing per connection
//!   (ready is the same at 16 and at 4096), so the idle cost was
//!   measured with connections open: (84 557 824 - 9 228 288) / 4080 =
//!   18 463.1 bytes on glibc, (95 010 816 - 8 663 040) / 4080 = 21 163.7
//!   on musl, rounded up 18 464 and 21 164; less the budgeted client
//!   buffers (16 384 receive + 1 024 send = 17 408), 1 056 and 3 756
//!   bytes of per-entry state and allocator rounding (musl's size
//!   classes). That was the whole allowance until the review's C1 (5 KiB
//!   with the refused connection's send buffer); the request in flight
//!   is measured below.
//! - Per disk thread: (139 591 680 - 9 228 288) / 60 = 2 172 723 bytes
//!   on glibc, the 2 MiB stack plus 75 571 (73.8 KiB); on musl
//!   (135 516 160 - 8 663 040) / 60 = 2 114 219, the stack plus 17 067
//!   (16.7 KiB). `/proc/<pid>/maps` shows each thread's 12 KiB
//!   `sigaltstack` and glibc's 132 KiB arenas (31 for 64 threads here;
//!   glibc makes up to 8 x CPUs, so one per thread on 8 or more CPUs).
//!   [`DISK_THREAD_OVERHEAD`] is 256 KiB: one arena and the sigaltstack,
//!   144 KiB, with margin.
//! - Base: the loaded glibc reading less `memory_limit`, the 4 stacks
//!   and the connections' allowance,
//!   10 014 720 - 1 068 608 - 4 x 2 MiB - 16 x 5 KiB (the allowance
//!   then) = 475 584 (musl's is lower), rounded up to a MiB and
//!   doubled: 2 MiB. Less the thread term's 4 x 256 KiB too, the
//!   remainder would be negative (that term carries a margin), so the
//!   base stands on this non-stack figure and its margin. Doubled again to
//!   [`BASE_ALLOWANCE`], 4 MiB: allocator bookkeeping and glibc's
//!   main-arena top pad grow with none of the other terms, and 2 MiB of
//!   slack costs nothing, while 2 MiB too little aborts a server.
//!
//! # The request in flight (2026-11-09, stage 9 final fix wave)
//!
//! A connection's request keeps state outside the budget until it is
//! answered: the strings of its summary for the log (method, host, raw
//! path and query, all copied out of its head, so together at most
//! `max_head`) and, while a static open is pending, the decoded path,
//! one buffer of at most the request line (at most `max_head`), moved
//! from the core's `OpenFile` into the disk job, never copied. A file a
//! backend names (FR-PROXY-32) has a decoded path bounded by the
//! upstream's receive buffer, `max(32 KiB, max_head)`, not by `max_head`;
//! it is covered all the same, since `ExchangeBuffers::into_head` frees
//! that budgeted, larger buffer before the open is pending. Every
//! slot can hold it at once: the opens queue behind the disk workers
//! (a slow disk), or the exchanges wait on a stalled backend.
//!
//! Measured as `VmData` like the idle case, both libcs (musl linked
//! statically, `+crt-static`), the server as `nobody`, `memory_limit
//! 1GiB`, every one of N slots holding one request: a static `GET` of
//! `/a` repeated to the line's end (the most segments) whose open is
//! held pending (the disk workers' `openat2` delayed 6 s by `strace
//! -e inject=openat2:delay_enter=6000000`; no slot was answered when
//! read), or a proxied `POST` with that path to a backend that never
//! answers. Per slot, beyond `worst_case_per_connection` (the budget it
//! may fill), in bytes; N = 1000 for heads up to 16 KiB, 60 above
//! (glibc's 128 KiB top pad blurs N = 60 by about 2 KiB a slot):
//!
//! ```text
//! max_request_line / max_head    static            proxy
//!                              glibc    musl     glibc    musl
//!     64 /    128              1 416   1 801     1 306   6 717
//!    960 /  1 024              3 228   3 822     2 230   7 849
//!  4 032 /  4 096              9 310  10 580     5 306  11 716
//!  8 192 / 16 384             17 596  23 069     9 484  20 801
//! 65 472 / 65 536 (N 60)     128 751 157 560    67 493  96 028
//! 262 080 / 262 144 (N 60)   526 814 530 432   277 618 276 321
//! ```
//!
//! (The musl proxy floor, 6 717 at a 128-byte head, is mostly musl's
//! size-class rounding of the exchange's buffers, the 32 KiB receive
//! buffer above all, which the idle measurement never allocated. A short
//! request, `/a`, costs 238 to 369 bytes over an idle connection, so the
//! fixed part is per connection, not per request.)
//!
//! - [`PER_CONNECTION_FIXED`]: the floor at the smallest head, musl's
//!   proxy 6 717, rounded up to a KiB, plus the 1 KiB send buffer of a
//!   refused connection (`SEND_CAPACITY`, allocated on demand, outside
//!   the budget): 8 KiB. It covers the idle 3 756 too.
//! - [`REQUEST_STATE_FACTOR`]: what remains, per byte of `max_head`, at
//!   most (157 560 - 8 192) / 65 536 = 2.28 (musl, static: the summary's
//!   path and the decoded path, each about a line, in musl's size
//!   classes); glibc's worst is (526 814 - 8 192) / 262 144 = 1.98.
//!   Rounded up: 3. The bound behind it is two copies of at most
//!   `max_head` bytes each, so it holds for any host name and any
//!   segment count; at the default limits it adds 51 KiB per connection.

use durchlauferhitzer_core::config::Limits;

use super::Failure;
use crate::log::DriverLog;
use crate::sys::rlimit::{cap_data, data};

pub use crate::pool::DISK_THREAD_STACK;

/// What `memory_limit` leaves outside and grows with neither the
/// connections nor the disk workers (measured, module docs).
pub const BASE_ALLOWANCE: u64 = 4 * 1024 * 1024;

/// What each disk worker maps besides its [`DISK_THREAD_STACK`]: its
/// `sigaltstack` and, on glibc, an arena (measured, module docs). Since
/// startup limits glibc to one arena (`sys::malloc`, FR-PROC-04 step 6b)
/// no worker gets an arena of its own, so the arena part is slack now;
/// the constant is kept.
pub const DISK_THREAD_OVERHEAD: u64 = 256 * 1024;

/// What each counted connection holds outside `memory_limit` whatever its
/// request: its per-entry state and allocator rounding, and a refused
/// connection's send buffer (measured idle, module docs).
pub const PER_CONNECTION_FIXED: u64 = 8 * 1024;

/// What each counted connection may hold outside `memory_limit` for its
/// request in flight, in multiples of `max_head`: the strings a request
/// keeps for its log (method, host, raw path and query, all copied out of
/// its head, together at most `max_head`) and a static open's decoded
/// path (at most the request line, so at most `max_head`; a path a backend
/// names is bounded by the upstream receive buffer, `max(32 KiB,
/// max_head)`, which is freed, budget and all, before its open is
/// pending), with allocator rounding (measured with every slot holding the worst case,
/// module docs).
pub const REQUEST_STATE_FACTOR: u64 = 3;

/// The `RLIMIT_DATA` the backstop sets for `limits` (the formula in the
/// module docs); saturating.
pub fn limit(limits: &Limits) -> u64 {
    let stack = u64::try_from(DISK_THREAD_STACK).unwrap_or(u64::MAX);
    let max_head = u64::try_from(limits.max_head).unwrap_or(u64::MAX);
    let per_connection =
        PER_CONNECTION_FIXED.saturating_add(REQUEST_STATE_FACTOR.saturating_mul(max_head));
    limits
        .memory_limit
        .saturating_add(BASE_ALLOWANCE)
        .saturating_add(
            u64::from(limits.disk_threads)
                .saturating_mul(stack.saturating_add(DISK_THREAD_OVERHEAD)),
        )
        .saturating_add(u64::from(limits.max_connections).saturating_mul(per_connection))
}

/// Sets the soft and hard `RLIMIT_DATA` to `needed` ([`limit`] of the
/// config's limits); a hard limit below it fails naming both numbers. The note is the startup log's
/// `memory_backstop` line.
pub(super) fn apply(needed: u64) -> Result<DriverLog, Failure> {
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
        BASE_ALLOWANCE, DISK_THREAD_OVERHEAD, DISK_THREAD_STACK, PER_CONNECTION_FIXED,
        REQUEST_STATE_FACTOR, limit,
    };

    #[test]
    fn limit_is_memory_limit_plus_the_three_allowance_terms() {
        let limits = Limits {
            memory_limit: 1 << 20,
            disk_threads: 4,
            max_connections: 16,
            max_head: 16 << 10,
            ..Limits::default()
        };
        assert_eq!(DISK_THREAD_STACK, 2 << 20);
        assert_eq!(REQUEST_STATE_FACTOR, 3);
        assert_eq!(
            limit(&limits),
            (1 << 20)
                + BASE_ALLOWANCE
                + 4 * ((2 << 20) + DISK_THREAD_OVERHEAD)
                + 16 * (PER_CONNECTION_FIXED + 3 * (16 << 10))
        );
    }

    /// The request-state term follows `max_head`, not a constant: a
    /// longer head allows more per-request state (the stage 9 review's
    /// C1).
    #[test]
    fn the_per_connection_term_grows_with_max_head() {
        let at = |max_head| {
            limit(&Limits {
                max_connections: 100,
                max_head,
                max_request_line: 8 << 10,
                ..Limits::default()
            })
        };
        assert_eq!(
            at(1 << 20) - at(16 << 10),
            100 * 3 * ((1 << 20) - (16 << 10))
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
