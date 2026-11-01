//! seccomp (FR-PROC-06): installing a filter for every thread, and the
//! probe for whether the kernel has it.

use std::io;

use super::fd::set_no_new_privs;

/// `__AUDIT_ARCH_64BIT | __AUDIT_ARCH_LE` (`linux/audit.h`).
const AUDIT_64BIT_LE: u32 = 0x8000_0000 | 0x4000_0000;

/// `seccomp_data.arch` on x86_64: `AUDIT_ARCH_X86_64`.
pub const AUDIT_ARCH_X86_64: u32 = AUDIT_64BIT_LE | libc::EM_X86_64 as u32;

/// `seccomp_data.arch` on aarch64: `AUDIT_ARCH_AARCH64`.
pub const AUDIT_ARCH_AARCH64: u32 = AUDIT_64BIT_LE | libc::EM_AARCH64 as u32;

/// Whether the kernel can run the server's filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// Filters with both of the server's actions can be installed.
    Supported,
    /// No seccomp (`ENOSYS`), no filter mode (`EINVAL`), or an action the
    /// kernel lacks (`EOPNOTSUPP`), or seccomp itself is filtered.
    Missing,
}

/// Probes the filter's two actions, `SECCOMP_RET_ERRNO` (`fail`) and
/// `SECCOMP_RET_KILL_PROCESS` (`kill`), with `SECCOMP_GET_ACTION_AVAIL`.
pub fn supported() -> Support {
    let available = |action: u32| {
        // SAFETY: `action` is a readable `u32` that outlives the call; the
        // kernel only reads it.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_GET_ACTION_AVAIL,
                0 as libc::c_uint,
                &raw const action,
            )
        };
        ret == 0
    };
    if available(libc::SECCOMP_RET_ERRNO) && available(libc::SECCOMP_RET_KILL_PROCESS) {
        Support::Supported
    } else {
        Support::Missing
    }
}

/// Installs `program` for every thread of the process: first
/// `PR_SET_NO_NEW_PRIVS` (so no privilege is needed), then
/// `SECCOMP_SET_MODE_FILTER` with `SECCOMP_FILTER_FLAG_TSYNC` (every
/// thread, at once) and `SECCOMP_FILTER_FLAG_LOG` (the kernel logs each
/// call the filter does not allow). A thread that cannot be synchronized
/// (it has a different filter of its own) fails the call, naming it, and
/// nothing is installed. An empty program, or one longer than
/// `BPF_MAXINSNS`, is `InvalidInput`.
pub fn install(program: &[libc::sock_filter]) -> io::Result<()> {
    let len = u16::try_from(program.len())
        .ok()
        .filter(|&len| len > 0 && i32::from(len) <= libc::BPF_MAXINSNS)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a filter of {} instructions", program.len()),
            )
        })?;
    set_no_new_privs()?;
    let prog = libc::sock_fprog {
        len,
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: `prog` points to `len` valid instructions in `program`,
    // which outlives the call; the kernel only reads them (it copies the
    // filter).
    let ret = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC | libc::SECCOMP_FILTER_FLAG_LOG,
            &raw const prog,
        )
    };
    match ret {
        0 => Ok(()),
        -1 => Err(io::Error::last_os_error()),
        // TSYNC: the id of the thread that could not be synchronized.
        tid => Err(io::Error::other(format!(
            "seccomp: thread {tid} cannot be synchronized"
        ))),
    }
}
