//! `RLIMIT_NOFILE`, the descriptor budget (NFR-RES-05), and
//! `RLIMIT_DATA`, the memory backstop (NFR-RES-09).

use std::io;

use super::fd::cvt;

/// The resources this module reads and sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resource {
    /// `RLIMIT_NOFILE`.
    Nofile,
    /// `RLIMIT_DATA`.
    Data,
}

fn get(resource: Resource) -> io::Result<libc::rlimit> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let ret = match resource {
        // SAFETY: `limit` is a writable `rlimit`.
        Resource::Nofile => unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        // SAFETY: `limit` is a writable `rlimit`.
        Resource::Data => unsafe { libc::getrlimit(libc::RLIMIT_DATA, &mut limit) },
    };
    cvt(ret)?;
    Ok(limit)
}

fn set(resource: Resource, limit: &libc::rlimit) -> io::Result<()> {
    let ret = match resource {
        // SAFETY: `limit` is a valid `rlimit` the kernel only reads.
        Resource::Nofile => unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, limit) },
        // SAFETY: `limit` is a valid `rlimit` the kernel only reads.
        Resource::Data => unsafe { libc::setrlimit(libc::RLIMIT_DATA, limit) },
    };
    cvt(ret)?;
    Ok(())
}

/// The soft and hard `RLIMIT_NOFILE` (`RLIM_INFINITY` is `u64::MAX`).
pub fn nofile() -> io::Result<(u64, u64)> {
    let limit = get(Resource::Nofile)?;
    Ok((limit.rlim_cur, limit.rlim_max))
}

/// Raises the soft `RLIMIT_NOFILE` to `to`, keeping the hard limit. A `to`
/// at or below the current soft limit changes nothing; one above the hard
/// limit fails (`EINVAL`, or `EPERM` past the system maximum).
pub fn raise_nofile(to: u64) -> io::Result<()> {
    let limit = get(Resource::Nofile)?;
    if to <= limit.rlim_cur {
        return Ok(());
    }
    set_soft_nofile(to)
}

/// Sets the soft `RLIMIT_NOFILE` to `to`, lower or higher, keeping the hard
/// limit; one above the hard limit fails (`EINVAL`) and changes nothing.
/// The limit is process-wide: the driver's tests use it to run out of
/// descriptors on purpose, each in a binary of its own.
pub fn set_soft_nofile(to: u64) -> io::Result<()> {
    let mut limit = get(Resource::Nofile)?;
    limit.rlim_cur = to;
    set(Resource::Nofile, &limit)
}

/// The soft and hard `RLIMIT_DATA` (`RLIM_INFINITY` is `u64::MAX`).
pub fn data() -> io::Result<(u64, u64)> {
    let limit = get(Resource::Data)?;
    Ok((limit.rlim_cur, limit.rlim_max))
}

/// Sets the soft and the hard `RLIMIT_DATA` both to `to`. Lowering needs
/// no privilege; once lowered, the hard limit cannot be raised again
/// without `CAP_SYS_RESOURCE`. A `to` above the hard limit fails (`EPERM`
/// without that capability) and changes nothing.
pub fn cap_data(to: u64) -> io::Result<()> {
    let limit = libc::rlimit {
        rlim_cur: to,
        rlim_max: to,
    };
    set(Resource::Data, &limit)
}
