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
    let mut limit = get()?;
    if to <= limit.rlim_cur {
        return Ok(());
    }
    limit.rlim_cur = to;
    // SAFETY: `limit` is a valid `rlimit` the kernel only reads.
    cvt(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) })?;
    Ok(())
}
