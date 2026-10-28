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

/// The real, effective and saved user ids (`getresuid`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uids {
    pub real: u32,
    pub effective: u32,
    pub saved: u32,
}

/// `getresuid`.
pub fn uids() -> io::Result<Uids> {
    let (mut real, mut effective, mut saved) = (0, 0, 0);
    // SAFETY: the three pointers are to writable `uid_t`s.
    cvt(unsafe { libc::getresuid(&mut real, &mut effective, &mut saved) })?;
    Ok(Uids {
        real,
        effective,
        saved,
    })
}

/// Drops to `uid`/`gid` for good, from a process with some uid 0: takes
/// the effective uid back to 0 if it is not (allowed while the real or
/// saved uid is 0), then `setgroups([gid])`, `setgid(gid)`, `setuid(uid)`
/// (as effective root these set the real, effective and saved ids), and
/// checks that `setuid(0)` fails and that all three uids are `uid`.
/// Changes the whole process (glibc applies the ids to every thread);
/// never call it in a process whose privileges must stay.
pub fn drop_to(uid: u32, gid: u32) -> io::Result<()> {
    if uids()?.effective != 0 {
        // SAFETY: `seteuid` takes no pointers.
        cvt(unsafe { libc::seteuid(0) })?;
    }
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
    let after = uids()?;
    if after
        != (Uids {
            real: uid,
            effective: uid,
            saved: uid,
        })
    {
        return Err(io::Error::other(format!(
            "uids after the privilege drop: {after:?}"
        )));
    }
    Ok(())
}

/// The effective user id (`geteuid` cannot fail).
pub fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}
