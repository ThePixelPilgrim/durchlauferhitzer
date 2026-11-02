//! Landlock (FR-PROC-06): the ABI probe and restricting the process to
//! reading beneath its static roots.
//!
//! libc has no Landlock types or constants: they are declared here from
//! `linux/landlock.h`.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use super::fd::{cvt_long, set_no_new_privs};

/// The kernel's Landlock ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Abi {
    /// No usable Landlock: not built in (`ENOSYS`), disabled
    /// (`EOPNOTSUPP`), or the probe refused (e.g. `EPERM` from a
    /// container's seccomp filter).
    Missing,
    /// The highest ABI version the kernel supports (1 is Linux 5.14).
    Version(u32),
}

/// A right the ABI cannot restrict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// Truncation (`LANDLOCK_ACCESS_FS_TRUNCATE`, ABI 3).
    Truncate,
    /// `ioctl` on devices (`LANDLOCK_ACCESS_FS_IOCTL_DEV`, ABI 5).
    DeviceIoctl,
}

/// `LANDLOCK_CREATE_RULESET_VERSION`.
const CREATE_RULESET_VERSION: u32 = 1 << 0;

/// `LANDLOCK_RULE_PATH_BENEATH`.
const RULE_PATH_BENEATH: libc::c_int = 1;

/// `LANDLOCK_ACCESS_FS_READ_FILE`.
const READ_FILE: u64 = 1 << 2;

/// `LANDLOCK_ACCESS_FS_READ_DIR`.
const READ_DIR: u64 = 1 << 3;

/// ABI 1: `EXECUTE` (bit 0) to `MAKE_SYM` (bit 12).
const ABI_1_RIGHTS: u64 = (1 << 13) - 1;

/// `LANDLOCK_ACCESS_FS_REFER`, ABI 2.
const REFER: u64 = 1 << 13;

/// `LANDLOCK_ACCESS_FS_TRUNCATE`, ABI 3.
const TRUNCATE: u64 = 1 << 14;

/// `LANDLOCK_ACCESS_FS_IOCTL_DEV`, ABI 5.
const IOCTL_DEV: u64 = 1 << 15;

/// `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`, ABI 6.
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;

/// `LANDLOCK_SCOPE_SIGNAL`, ABI 6.
const SCOPE_SIGNAL: u64 = 1 << 1;

/// `struct landlock_ruleset_attr`. Before ABI 6 only `handled_access_fs`
/// is passed (the `size` argument stops there): the kernel reads the
/// later fields as 0. From ABI 6 the whole struct is passed, with no
/// network rule (`handled_access_net` 0, network rules are not part of
/// stage 5) and both scopes.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// The size of `struct landlock_ruleset_attr` that `abi` knows, up to
/// the last field the server sets: `handled_access_fs` alone before ABI
/// 6, all three from 6.
fn attr_size(abi: u32) -> usize {
    if abi >= 6 {
        size_of::<RulesetAttr>()
    } else {
        size_of::<u64>()
    }
}

/// The scopes `abi` knows: signals and abstract unix sockets limited to
/// the domain from ABI 6, none before.
fn scoped(abi: u32) -> u64 {
    if abi >= 6 {
        SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
    } else {
        0
    }
}

/// `struct landlock_path_beneath_attr`, which the kernel declares
/// `__attribute__((packed))`: `allowed_access` (`__u64`) at offset 0,
/// `parent_fd` (`__s32`) at offset 8, 12 bytes, no trailing padding.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The kernel's ABI, from `landlock_create_ruleset(NULL, 0,
/// LANDLOCK_CREATE_RULESET_VERSION)`. `ENOSYS` and `EOPNOTSUPP` are
/// [`Abi::Missing`]; so is any other error (the probe takes no input that
/// could be wrong).
pub fn abi() -> Abi {
    // SAFETY: with this flag the kernel takes a null attribute pointer and
    // a size of 0, and reads nothing.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0_usize,
            libc::c_long::from(CREATE_RULESET_VERSION),
        )
    };
    match u32::try_from(ret) {
        Ok(version) if version > 0 => Abi::Version(version),
        Ok(_) | Err(_) => Abi::Missing,
    }
}

/// Every filesystem right `abi` knows: ABI 1's thirteen, `REFER` from 2,
/// `TRUNCATE` from 3, `IOCTL_DEV` from 5 (ABI 4 and 6 to 7 add none).
fn handled_access(abi: u32) -> u64 {
    let mut rights = ABI_1_RIGHTS;
    if abi >= 2 {
        rights |= REFER;
    }
    if abi >= 3 {
        rights |= TRUNCATE;
    }
    if abi >= 5 {
        rights |= IOCTL_DEV;
    }
    rights
}

/// The rights `abi` cannot restrict: truncation before ABI 3, device
/// `ioctl` before ABI 5 (FR-PROC-06).
pub fn gaps(abi: u32) -> Vec<Gap> {
    let mut gaps = Vec::new();
    if abi < 3 {
        gaps.push(Gap::Truncate);
    }
    if abi < 5 {
        gaps.push(Gap::DeviceIoctl);
    }
    gaps
}

/// Restricts the calling thread, and every thread it starts afterwards,
/// to reading files and directories beneath `roots`: a ruleset handling
/// every filesystem right `abi` knows (and, from ABI 6, scoping signals
/// and abstract unix sockets to the domain), one `LANDLOCK_RULE_PATH_BENEATH`
/// rule per root allowing only `READ_FILE | READ_DIR` (added from the
/// descriptor, an `O_PATH` one is enough: no path lookup), then
/// `PR_SET_NO_NEW_PRIVS` and `landlock_restrict_self`. Threads that
/// already run are not restricted. `abi` must not name a filesystem
/// right the kernel lacks (pass the kernel's [`abi`] or less), or the
/// ruleset is `EINVAL`. No root: nothing is readable. Each error names
/// its step (and root, by index) and keeps its [`io::ErrorKind`]; a
/// failure of `landlock_restrict_self` leaves `no_new_privs` set without
/// a restriction.
pub fn restrict_to_reading(roots: &[BorrowedFd<'_>], abi: u32) -> io::Result<()> {
    let attr = RulesetAttr {
        handled_access_fs: handled_access(abi),
        handled_access_net: 0,
        scoped: scoped(abi),
    };
    // SAFETY: `attr` is a readable `RulesetAttr` of at least the size
    // passed that outlives the call; the kernel only reads it.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            attr_size(abi),
            0 as libc::c_long,
        )
    };
    let ruleset = descriptor(ret).map_err(step("landlock_create_ruleset"))?;
    for (i, root) in roots.iter().enumerate() {
        let rule = PathBeneathAttr {
            allowed_access: READ_FILE | READ_DIR,
            parent_fd: root.as_raw_fd(),
        };
        // SAFETY: `rule` is a readable `PathBeneathAttr` that outlives the
        // call, the kernel only reads it; both descriptors are open.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                libc::c_long::from(ruleset.as_raw_fd()),
                libc::c_long::from(RULE_PATH_BENEATH),
                &raw const rule,
                0 as libc::c_long,
            )
        };
        cvt_long(ret).map_err(step(&format!("landlock_add_rule (root {i})")))?;
    }
    set_no_new_privs().map_err(step("PR_SET_NO_NEW_PRIVS"))?;
    // SAFETY: `ruleset` is an open Landlock ruleset descriptor; no
    // pointers are passed.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_restrict_self,
            libc::c_long::from(ruleset.as_raw_fd()),
            0 as libc::c_long,
        )
    };
    cvt_long(ret).map_err(step("landlock_restrict_self"))?;
    Ok(())
}

/// Prefixes an error with the step that failed, keeping its kind.
fn step(what: &str) -> impl FnOnce(io::Error) -> io::Error + '_ {
    move |err| io::Error::new(err.kind(), format!("{what}: {err}"))
}

/// `syscall`'s `-1`-and-errno check for a call returning a new descriptor.
fn descriptor(ret: libc::c_long) -> io::Result<OwnedFd> {
    let fd = libc::c_int::try_from(cvt_long(ret)?)
        .map_err(|_| io::Error::other("landlock returned a descriptor beyond c_int"))?;
    // SAFETY: `fd` was just created by `landlock_create_ruleset` and
    // nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_1_and_2_cannot_restrict_truncation_or_device_ioctl() {
        assert_eq!(gaps(1), [Gap::Truncate, Gap::DeviceIoctl]);
        assert_eq!(gaps(2), [Gap::Truncate, Gap::DeviceIoctl]);
    }

    #[test]
    fn abi_3_and_4_cannot_restrict_device_ioctl() {
        assert_eq!(gaps(3), [Gap::DeviceIoctl]);
        assert_eq!(gaps(4), [Gap::DeviceIoctl]);
    }

    #[test]
    fn abi_5_and_later_have_no_gap() {
        for abi in [5, 6, 7, 8] {
            assert_eq!(gaps(abi), []);
        }
    }

    #[test]
    fn every_filesystem_right_the_abi_knows_is_handled() {
        assert_eq!(handled_access(1), 0x1fff);
        assert_eq!(handled_access(2), 0x3fff);
        assert_eq!(handled_access(3), 0x7fff);
        assert_eq!(handled_access(4), 0x7fff);
        assert_eq!(handled_access(5), 0xffff);
        assert_eq!(handled_access(7), 0xffff);
    }

    #[test]
    fn abi_6_and_later_scope_signals_and_abstract_unix_sockets() {
        for abi in [1, 2, 3, 4, 5] {
            assert_eq!(scoped(abi), 0);
            assert_eq!(attr_size(abi), 8);
        }
        for abi in [6, 7, 8] {
            assert_eq!(scoped(abi), 0b11);
            assert_eq!(attr_size(abi), 24);
        }
    }

    #[test]
    fn the_attributes_have_the_kernel_layout() {
        // `landlock_ruleset_attr`: three u64s; the packed
        // `landlock_path_beneath_attr`: a u64 then an s32, no padding.
        assert_eq!(size_of::<RulesetAttr>(), 24);
        assert_eq!(size_of::<PathBeneathAttr>(), 12);
        assert_eq!(align_of::<PathBeneathAttr>(), 1);
    }
}
