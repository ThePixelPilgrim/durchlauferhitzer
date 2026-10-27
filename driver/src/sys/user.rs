//! The user database and the privilege drop (FR-PROC-03, A8).

use std::ffi::CString;
use std::io;
use std::mem::zeroed;

use super::fd::cvt;

/// The largest `getpwnam_r` buffer tried before giving up (`ERANGE`).
const MAX_PASSWD_BUF: usize = 1 << 20;

/// Looks `name` up with `getpwnam_r`: its uid and primary gid, or `None` if
/// there is no such user. A NUL byte in `name` is `InvalidInput`.
pub fn lookup_user(name: &str) -> io::Result<Option<(u32, u32)>> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL byte in a user name"))?;
    let mut buf: Vec<libc::c_char> = vec![0; 1024];
    loop {
        // SAFETY: an all-zero `passwd` is a valid value (null pointers).
        let mut pwd: libc::passwd = unsafe { zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `name` is NUL-terminated; `pwd`, `buf` (for `buf.len()`
        // bytes) and `result` are writable. The strings `pwd` points to live
        // in `buf`; only the integer fields are read below.
        let rc = unsafe {
            libc::getpwnam_r(
                name.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_PASSWD_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result.is_null() {
            return Ok(None);
        }
        return Ok(Some((pwd.pw_uid, pwd.pw_gid)));
    }
}

/// Drops to `uid`/`gid` for good: `setgroups([gid])`, `setgid(gid)`,
/// `setuid(uid)`, then checks that `setuid(0)` fails. Changes the whole
/// process (glibc applies the ids to every thread); never call it in a
/// process whose privileges must stay.
pub fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    let groups = [gid];
    // SAFETY: `groups` is readable for the one entry passed.
    cvt(unsafe { libc::setgroups(groups.len(), groups.as_ptr()) })?;
    // SAFETY: `setgid` takes no pointers.
    cvt(unsafe { libc::setgid(gid) })?;
    // SAFETY: `setuid` takes no pointers.
    cvt(unsafe { libc::setuid(uid) })?;
    // SAFETY: `setuid` takes no pointers.
    if unsafe { libc::setuid(0) } == 0 {
        return Err(io::Error::other(
            "setuid(0) succeeded after the privilege drop",
        ));
    }
    Ok(())
}

/// The effective user id (`geteuid` cannot fail).
pub fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}
