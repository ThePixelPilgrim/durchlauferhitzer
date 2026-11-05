//! A test-only `LD_PRELOAD` library, never part of the product: its
//! `clock_gettime` makes the system call every time, as the libc's vDSO
//! does where the clocksource has no vDSO mode (hpet, acpi_pm, Xen,
//! arm64 errata). `containment.rs` builds it with `rustc` (it is no
//! cargo target) and runs the server with it under `seccomp_action
//! kill`.

use std::ffi::{c_int, c_long, c_void};

unsafe extern "C" {
    /// The libc's `syscall(2)` (glibc or musl).
    fn syscall(number: c_long, ...) -> c_long;
}

#[cfg(target_arch = "x86_64")]
const SYS_CLOCK_GETTIME: c_long = 228;
#[cfg(target_arch = "aarch64")]
const SYS_CLOCK_GETTIME: c_long = 113;

/// `clock_gettime(3)` through the system call.
///
/// # Safety
///
/// As `clock_gettime(3)`: `ts` is writable for a `struct timespec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn clock_gettime(clock: c_int, ts: *mut c_void) -> c_int {
    // SAFETY: the kernel writes at most one `struct timespec` to `ts`,
    // which the caller guarantees is writable; `syscall` sets errno.
    let ret = unsafe { syscall(SYS_CLOCK_GETTIME, c_long::from(clock), ts) };
    c_int::try_from(ret).unwrap_or(-1)
}
