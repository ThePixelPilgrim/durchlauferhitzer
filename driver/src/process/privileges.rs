//! The privilege rules of startup (FR-PROC-03, A8).
//!
//! - No check of the port numbers (owner decision 2026-10-28, O7): a
//!   bind the kernel refuses (`EACCES` below 1024 as non-root) is the
//!   startup error naming its address (A6).
//! - After binding: as root, `user` is required (never root after
//!   binding) and the process drops to it for good (`drop_to`); as
//!   non-root nothing is dropped, and a set `user` is logged as kept.

use super::Failure;
use crate::log::{DriverLog, KeptReason};
use crate::sys::user::{drop_to, euid, lookup_user};

/// A8, the drop (A11 step 7): returns the entry to log once the log sink
/// exists, if any.
pub(super) fn drop_privileges(user: Option<&str>) -> Result<Option<DriverLog>, Failure> {
    let root = euid() == 0;
    match (root, user) {
        (false, None) => Ok(None),
        (false, Some(_)) => Ok(Some(DriverLog::PrivilegesKept {
            reason: KeptReason::NotRoot,
        })),
        (true, None) => Err(Failure(
            "running as root without `user`: the server never keeps root after binding".to_owned(),
        )),
        (true, Some(name)) => {
            let (uid, gid) = match lookup_user(name) {
                Ok(Some(ids)) => ids,
                Ok(None) => return Err(Failure(format!("user {name}: no such user"))),
                Err(err) => return Err(Failure(format!("user {name}: {err}"))),
            };
            drop_to(uid, gid).map_err(|err| {
                Failure(format!(
                    "cannot drop privileges to {name} (uid {uid}, gid {gid}): {err}"
                ))
            })?;
            Ok(None)
        }
    }
}
