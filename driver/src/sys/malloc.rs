//! glibc's malloc tunables. glibc creates a new arena the first time
//! contention forces one, and computes its arena limit then by
//! `__get_nprocs()`, which opens `/sys/devices/system/cpu/online` (and
//! `/proc/stat` as a fallback) with `openat` from whichever thread
//! allocates: a disk worker, after the seccomp filter, which allows no
//! `openat` (FR-PROC-06). Presetting the limit before any thread exists
//! means glibc never asks. musl's malloc has no arenas: nothing to do.

/// Limits glibc's malloc to one arena (`M_ARENA_MAX`), so that no thread
/// ever computes the limit itself (see the module docs). Called once at
/// startup, while the process has one thread. `false` when glibc
/// refuses (never for this value); startup then fails, since the filter
/// would end the process on the first such read.
#[cfg(target_env = "gnu")]
pub fn limit_arenas() -> bool {
    // SAFETY: `mallopt` only sets a malloc tunable from two integers; it
    // takes no pointers and is called before any other thread exists.
    unsafe { libc::mallopt(libc::M_ARENA_MAX, 1) != 0 }
}

/// musl: no arenas, nothing to limit.
#[cfg(not(target_env = "gnu"))]
pub fn limit_arenas() -> bool {
    true
}
