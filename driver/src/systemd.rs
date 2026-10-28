//! systemd socket activation (FR-PROC-02, A7): the listening sockets
//! systemd passed, checked against the config's `listen` addresses.
//!
//! - `LISTEN_PID` must be this process's pid and `LISTEN_FDS` at least 1;
//!   the descriptors are 3, 4, ... (`LISTEN_FDNAMES` is ignored). Neither
//!   variable set means no socket activation (the server binds itself).
//!   One set without the other, or a value that is not a number, is a
//!   startup error.
//! - Each descriptor must be a listening `AF_INET`/`AF_INET6` stream
//!   socket, bound to one of the `listen` addresses, and every `listen`
//!   address must have one (so there are never more sockets than
//!   addresses).
//! - The three variables are removed from the environment once read.

use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd, RawFd};

use crate::sys::env::remove_vars;
use crate::sys::fd::{Blocking, claim_inherited, set_status_flags, status_flags};
use crate::sys::socket::{Listening, is_listening_stream, local_addr};

/// The first descriptor systemd passes (`SD_LISTEN_FDS_START`).
const FIRST_FD: RawFd = 3;

const LISTEN_PID: &str = "LISTEN_PID";
const LISTEN_FDS: &str = "LISTEN_FDS";
const LISTEN_FDNAMES: &str = "LISTEN_FDNAMES";

/// Whether systemd passed sockets.
#[derive(Debug)]
pub(crate) enum Activation {
    /// Neither `LISTEN_PID` nor `LISTEN_FDS` is set.
    None,
    /// The passed sockets, non-blocking, in descriptor order.
    Sockets(Vec<OwnedFd>),
}

/// Reads and removes the systemd variables and takes the sockets they
/// announce, checked against `listen`. The `Err` is the startup error's
/// message.
pub(crate) fn take_sockets(listen: &[SocketAddr]) -> Result<Activation, String> {
    let pid = std::env::var_os(LISTEN_PID);
    let fds = std::env::var_os(LISTEN_FDS);
    if pid.is_none() && fds.is_none() {
        return Ok(Activation::None);
    }
    remove_vars(&[LISTEN_PID, LISTEN_FDS, LISTEN_FDNAMES])
        .map_err(|err| format!("cannot clear the systemd variables: {err}"))?;
    let number = |name: &str, value: Option<std::ffi::OsString>| -> Result<u32, String> {
        let value = value.ok_or_else(|| format!("{name} is not set, but the other one is"))?;
        value
            .to_str()
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| format!("{name} is not a number: {value:?}"))
    };
    let pid = number(LISTEN_PID, pid)?;
    let count = number(LISTEN_FDS, fds)?;
    let me = std::process::id();
    if pid != me {
        return Err(format!("{LISTEN_PID} is {pid}, not this process ({me})"));
    }
    if count == 0 {
        return Err(format!("{LISTEN_FDS} is 0"));
    }
    let too_many = usize::try_from(count).map_or(true, |count| count > listen.len());
    if too_many {
        return Err(format!(
            "systemd passed {count} sockets for {} listen addresses",
            listen.len()
        ));
    }
    let mut sockets = Vec::new();
    let mut bound = Vec::new();
    for fd in (0..count).filter_map(|i| RawFd::try_from(i).ok()) {
        let fd = FIRST_FD + fd;
        let (socket, addr) = take_one(fd, listen)?;
        if bound.contains(&addr) {
            return Err(format!("two systemd sockets are bound to {addr}"));
        }
        bound.push(addr);
        sockets.push(socket);
    }
    if let Some(missing) = listen.iter().find(|addr| !bound.contains(addr)) {
        return Err(format!("listen {missing} has no socket from systemd"));
    }
    Ok(Activation::Sockets(sockets))
}

/// Claims descriptor `fd`, checks it, and makes it non-blocking.
fn take_one(fd: RawFd, listen: &[SocketAddr]) -> Result<(OwnedFd, SocketAddr), String> {
    let socket = claim_inherited(fd).map_err(|err| format!("systemd socket fd {fd}: {err}"))?;
    let not_listening = || format!("systemd socket fd {fd} is not a listening TCP socket");
    match is_listening_stream(socket.as_fd()) {
        Ok(Listening::Yes) => {}
        Ok(Listening::No) => return Err(not_listening()),
        Err(err) => return Err(format!("systemd socket fd {fd}: {err}")),
    }
    let addr =
        local_addr(socket.as_fd()).map_err(|err| format!("systemd socket fd {fd}: {err}"))?;
    if !listen.contains(&addr) {
        return Err(format!(
            "systemd socket fd {fd} is bound to {addr}, which is not a listen address"
        ));
    }
    let flags = status_flags(socket.as_fd())
        .and_then(|flags| set_status_flags(socket.as_fd(), flags.with(Blocking::NonBlocking)))
        .map_err(|err| format!("systemd socket fd {fd}: {err}"));
    flags?;
    Ok((socket, addr))
}
