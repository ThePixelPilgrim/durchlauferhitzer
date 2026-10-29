//! The privilege rules of startup (FR-PROC-03, A8).
//!
//! - No check of the port numbers (owner decision 2026-10-28, O7): a
//!   bind the kernel refuses (`EACCES` below 1024 as non-root) is the
//!   startup error naming its address (A6).
//! - Root is any uid 0, real, effective or saved (owner decision
//!   2026-10-28, O6): such a process could become root again.
//! - After binding: as root, `user` is required (never root after
//!   binding), its primary group must not be 0 (owner decision
//!   2026-10-29, OA3: never group root either), and the process drops to
//!   it for good (`drop_to`); as
//!   non-root the ids are kept, and a set `user` is logged as kept.
//! - Then, root or not, every capability is cleared (owner decision
//!   2026-10-29, OA2): the ambient set, then the permitted, effective and
//!   inheritable sets, checked with `capget`. Capabilities are per
//!   thread: this runs while the process has one thread (A11 step 7,
//!   before the signal mask and the disk workers), so every later
//!   thread starts with none.

use super::Failure;
use crate::log::{DriverLog, KeptReason};
use crate::sys::caps::{Capabilities, capabilities, clear_ambient, clear_capabilities};
use crate::sys::user::{Uids, drop_to, lookup_user, uids};

/// A8, the drop (A11 step 7), then OA2, the capabilities: returns the
/// entry to log once the log sink exists, if any.
pub(super) fn drop_privileges(user: Option<&str>) -> Result<Option<DriverLog>, Failure> {
    let note = drop_ids(user)?;
    clear_all_capabilities()?;
    Ok(note)
}

/// OA2: no capability is left in any set of this (the only) thread.
fn clear_all_capabilities() -> Result<(), Failure> {
    clear_ambient().map_err(|err| {
        Failure(format!(
            "cannot clear the ambient capabilities (prctl PR_CAP_AMBIENT_CLEAR_ALL): {err}"
        ))
    })?;
    clear_capabilities()
        .map_err(|err| Failure(format!("cannot clear the capabilities (capset): {err}")))?;
    match capabilities() {
        Ok(Capabilities::NONE) => Ok(()),
        Ok(left) => Err(Failure(format!(
            "capabilities left after clearing them (capget): permitted {:#x}, \
             effective {:#x}, inheritable {:#x}",
            left.permitted, left.effective, left.inheritable
        ))),
        Err(err) => Err(Failure(format!(
            "cannot read the capabilities (capget): {err}"
        ))),
    }
}

/// A8: drops to `user` as root; keeps the ids as non-root.
fn drop_ids(user: Option<&str>) -> Result<Option<DriverLog>, Failure> {
    let ids = uids().map_err(|err| Failure(format!("getresuid: {err}")))?;
    match (privilege(ids), user) {
        (Privilege::NotRoot, None) => Ok(None),
        (Privilege::NotRoot, Some(_)) => Ok(Some(DriverLog::PrivilegesKept {
            reason: KeptReason::NotRoot,
        })),
        (Privilege::Root, None) => Err(Failure(
            "running as root without `user`: the server never keeps root after binding".to_owned(),
        )),
        (Privilege::Root, Some(name)) => {
            let (uid, gid) = match lookup_user(name) {
                Ok(Some(ids)) => ids,
                Ok(None) => return Err(Failure(format!("user {name}: no such user"))),
                Err(err) => return Err(Failure(format!("user {name}: {err}"))),
            };
            if gid == 0 {
                return Err(Failure(format!(
                    "user {name}: its primary group is 0 (root); \
                     the server never keeps group root after binding"
                )));
            }
            drop_to(uid, gid).map_err(|err| {
                Failure(format!(
                    "cannot drop privileges to {name} (uid {uid}, gid {gid}): {err}"
                ))
            })?;
            Ok(None)
        }
    }
}

/// Whether the process counts as root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Privilege {
    /// Some uid (real, effective or saved) is 0 (O6).
    Root,
    NotRoot,
}

fn privilege(ids: Uids) -> Privilege {
    if ids.real == 0 || ids.effective == 0 || ids.saved == 0 {
        Privilege::Root
    } else {
        Privilege::NotRoot
    }
}

#[cfg(test)]
mod tests {
    use super::{Privilege, privilege};
    use crate::sys::user::Uids;

    #[test]
    fn root_is_any_uid_0() {
        let ids = |real, effective, saved| Uids {
            real,
            effective,
            saved,
        };
        assert_eq!(privilege(ids(0, 0, 0)), Privilege::Root);
        assert_eq!(privilege(ids(0, 1, 1)), Privilege::Root);
        assert_eq!(privilege(ids(1, 0, 1)), Privilege::Root);
        assert_eq!(privilege(ids(1, 1, 0)), Privilege::Root);
        assert_eq!(privilege(ids(1, 2, 3)), Privilege::NotRoot);
    }
}
