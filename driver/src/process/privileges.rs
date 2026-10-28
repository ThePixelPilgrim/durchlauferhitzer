//! The privilege rules of startup (FR-PROC-03, A8).
//!
//! - Without systemd sockets, a `listen` port below 1024 needs root: as
//!   non-root that is a startup error before anything is bound.
//! - After binding: as root, `user` is required (never root after
//!   binding) and the process drops to it for good (`drop_to`); as
//!   non-root nothing is dropped, and a set `user` is logged as kept.

use std::net::SocketAddr;

use super::Failure;
use crate::log::{DriverLog, KeptReason};
use crate::sys::user::{drop_to, euid, lookup_user};

/// The first port that needs no root.
const UNPRIVILEGED_PORTS: u16 = 1024;

/// A8, first rule: checked when the process binds itself (no systemd
/// sockets), before it does.
pub(super) fn check_ports(listen: &[SocketAddr]) -> Result<(), Failure> {
    let uid = euid();
    if uid == 0 {
        return Ok(());
    }
    match listen.iter().find(|addr| addr.port() < UNPRIVILEGED_PORTS) {
        Some(addr) => Err(Failure(format!(
            "listen {addr} needs root (a port below 1024) or a socket from systemd, \
             but the effective uid is {uid}"
        ))),
        None => Ok(()),
    }
}

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
