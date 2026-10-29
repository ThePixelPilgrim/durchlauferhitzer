//! The calling thread's capabilities (FR-PROC-03; owner decision
//! 2026-10-29, OA2): cleared after binding, so the server never keeps a
//! capability, root or not.
//!
//! Capabilities belong to a thread, not to the process: these calls read
//! and change the calling thread's sets only. Startup clears them while
//! the process has one thread, so every thread started later (the disk
//! workers) inherits the empty sets.

use std::io;

use super::fd::{cvt, cvt_retry_ssize};

/// The three capability sets of a thread (`capget`), one bit per
/// capability (bit `n` is capability `n`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub permitted: u64,
    pub effective: u64,
    pub inheritable: u64,
}

impl Capabilities {
    /// No capability in any set.
    pub const NONE: Capabilities = Capabilities {
        permitted: 0,
        effective: 0,
        inheritable: 0,
    };
}

/// `_LINUX_CAPABILITY_VERSION_3` (`linux/capability.h`): 64-bit sets,
/// passed as two 32-bit words each.
const VERSION_3: u32 = 0x2008_0522;

/// `struct __user_cap_header_struct`.
#[repr(C)]
struct Header {
    version: u32,
    /// 0: the calling thread.
    pid: libc::c_int,
}

/// `struct __user_cap_data_struct`: one 32-bit word of each set.
#[repr(C)]
#[derive(Clone, Copy)]
struct Data {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const EMPTY: [Data; 2] = [Data {
    effective: 0,
    permitted: 0,
    inheritable: 0,
}; 2];

fn header() -> Header {
    Header {
        version: VERSION_3,
        pid: 0,
    }
}

/// Empties the calling thread's ambient set
/// (`prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL)`); always allowed.
pub fn clear_ambient() -> io::Result<()> {
    let zero: libc::c_ulong = 0;
    // SAFETY: `prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0)`
    // takes no pointers; the kernel requires the three unused arguments
    // to be 0.
    let ret = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::c_ulong::from(libc::PR_CAP_AMBIENT_CLEAR_ALL.unsigned_abs()),
            zero,
            zero,
            zero,
        )
    };
    cvt(ret).map(|_| ())
}

/// Empties the calling thread's permitted, effective and inheritable sets
/// (`capset`); dropping capabilities is always allowed. The ambient set,
/// a subset of the permitted one, is empty afterwards too.
pub fn clear_capabilities() -> io::Result<()> {
    let header = header();
    let data = EMPTY;
    // SAFETY: `header` and `data` are a valid version-3 header and the
    // two data words that version takes; the kernel only reads them,
    // during the call.
    cvt_retry_ssize(|| unsafe {
        libc::syscall(libc::SYS_capset, &raw const header, data.as_ptr())
    })?;
    Ok(())
}

/// The calling thread's capability sets (`capget`).
pub fn capabilities() -> io::Result<Capabilities> {
    let mut header = header();
    let mut data = EMPTY;
    // SAFETY: `header` is a valid version-3 header and `data` the two
    // writable data words that version fills; both outlive the call.
    cvt_retry_ssize(|| unsafe {
        libc::syscall(libc::SYS_capget, &raw mut header, data.as_mut_ptr())
    })?;
    let [low, high] = data;
    let join = |low: u32, high: u32| u64::from(high) << 32 | u64::from(low);
    Ok(Capabilities {
        permitted: join(low.permitted, high.permitted),
        effective: join(low.effective, high.effective),
        inheritable: join(low.inheritable, high.inheritable),
    })
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::{Capabilities, capabilities, clear_ambient, clear_capabilities};
    use crate::sys::user::euid;

    /// The four capability fields of `/proc/thread-self/status`, in hex
    /// as the kernel prints them: CapInh, CapPrm, CapEff, CapAmb.
    fn proc_caps() -> Vec<u64> {
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"]
            .iter()
            .map(|key| {
                let hex = status
                    .lines()
                    .find_map(|line| line.strip_prefix(key))
                    .unwrap()
                    .trim();
                u64::from_str_radix(hex, 16).unwrap()
            })
            .collect()
    }

    /// OA2: the calls agree with the kernel's own view, and clearing
    /// empties every set. Run on a thread of its own: capabilities are
    /// per thread, so the other tests keep theirs.
    #[test]
    fn clearing_empties_every_set_of_the_thread() {
        thread::spawn(|| {
            let before = capabilities().unwrap();
            let proc = proc_caps();
            assert_eq!(
                [before.inheritable, before.permitted, before.effective],
                proc[..3],
                "capget disagrees with /proc"
            );
            if euid() == 0 {
                assert_ne!(before.permitted, 0, "root without capabilities");
            }
            clear_ambient().unwrap();
            clear_capabilities().unwrap();
            assert_eq!(capabilities().unwrap(), Capabilities::NONE);
            assert_eq!(proc_caps(), [0, 0, 0, 0]);
            // Clearing what is clear already is fine.
            clear_ambient().unwrap();
            clear_capabilities().unwrap();
        })
        .join()
        .unwrap();
    }
}
