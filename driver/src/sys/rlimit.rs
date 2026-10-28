//! `RLIMIT_NOFILE`, the descriptor budget (NFR-RES-05).

use std::io;

use super::fd::cvt;

fn get() -> io::Result<libc::rlimit> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a writable `rlimit`.
    cvt(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) })?;
    Ok(limit)
}

/// The soft and hard `RLIMIT_NOFILE` (`RLIM_INFINITY` is `u64::MAX`).
pub fn nofile() -> io::Result<(u64, u64)> {
    let limit = get()?;
    Ok((limit.rlim_cur, limit.rlim_max))
}

/// Raises the soft `RLIMIT_NOFILE` to `to`, keeping the hard limit. A `to`
/// at or below the current soft limit changes nothing; one above the hard
/// limit fails (`EINVAL`, or `EPERM` past the system maximum).
pub fn raise_nofile(to: u64) -> io::Result<()> {
    let limit = get()?;
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
    let mut limit = get()?;
    limit.rlim_cur = to;
    // SAFETY: `limit` is a valid `rlimit` the kernel only reads.
    cvt(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) })?;
    Ok(())
}
