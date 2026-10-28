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

use std::ffi::OsStr;
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd, RawFd};

use crate::sys::env::remove_vars;
use crate::sys::fd::{Blocking, claim_inherited, set_status_flags, status_flags};
use crate::sys::socket::{Listening, V6Only, ipv6_only, is_listening_stream, local_addr};

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

/// What the activation variables announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Announced {
    /// Neither variable is set.
    Nothing,
    /// This many sockets, from fd 3 on (at least 1, at most one per
    /// `listen` address).
    Sockets(u32),
}

/// Reads and removes the systemd variables and takes the sockets they
/// announce, checked against `listen`. The `Err` is the startup error's
/// message.
pub(crate) fn take_sockets(listen: &[SocketAddr]) -> Result<Activation, String> {
    let pid = std::env::var_os(LISTEN_PID);
    let fds = std::env::var_os(LISTEN_FDS);
    if pid.is_some() || fds.is_some() {
        remove_vars(&[LISTEN_PID, LISTEN_FDS, LISTEN_FDNAMES])
            .map_err(|err| format!("cannot clear the systemd variables: {err}"))?;
    }
    let count = match parse_env(
        pid.as_deref(),
        fds.as_deref(),
        std::process::id(),
        listen.len(),
    )? {
        Announced::Nothing => return Ok(Activation::None),
        Announced::Sockets(count) => count,
    };
    let mut sockets = Vec::new();
    let mut bound = Vec::new();
    for fd in (0..count).filter_map(|i| RawFd::try_from(i).ok()) {
        let (socket, addr) = take_one(FIRST_FD + fd, listen)?;
        bound.push(addr);
        sockets.push(socket);
    }
    check_coverage(&bound, listen)?;
    Ok(Activation::Sockets(sockets))
}

/// The strict reading of the activation variables (A7; owner decision
/// 2026-10-28, O2): `me` is this process's pid, `addresses` the number
/// of `listen` addresses.
fn parse_env(
    pid: Option<&OsStr>,
    fds: Option<&OsStr>,
    me: u32,
    addresses: usize,
) -> Result<Announced, String> {
    if pid.is_none() && fds.is_none() {
        return Ok(Announced::Nothing);
    }
    let number = |name: &str, value: Option<&OsStr>| -> Result<u32, String> {
        let value = value.ok_or_else(|| format!("{name} is not set, but the other one is"))?;
        value
            .to_str()
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| format!("{name} is not a number: {value:?}"))
    };
    let pid = number(LISTEN_PID, pid)?;
    let count = number(LISTEN_FDS, fds)?;
    if pid != me {
        return Err(format!("{LISTEN_PID} is {pid}, not this process ({me})"));
    }
    if count == 0 {
        return Err(format!("{LISTEN_FDS} is 0"));
    }
    if usize::try_from(count).map_or(true, |count| count > addresses) {
        return Err(format!(
            "systemd passed {count} sockets for {addresses} listen addresses"
        ));
    }
    Ok(Announced::Sockets(count))
}

/// Each socket on its own address (in fd order) and every `listen`
/// address covered.
fn check_coverage(bound: &[SocketAddr], listen: &[SocketAddr]) -> Result<(), String> {
    for (i, addr) in bound.iter().enumerate() {
        if bound.iter().take(i).any(|earlier| earlier == addr) {
            return Err(format!("two systemd sockets are bound to {addr}"));
        }
    }
    match listen.iter().find(|addr| !bound.contains(addr)) {
        Some(missing) => Err(format!("listen {missing} has no socket from systemd")),
        None => Ok(()),
    }
}

/// Owner decision 2026-10-28 (O4): an IPv6 socket must accept IPv6
/// only, as the server's own IPv6 listeners do (FR-CFG-10); an IPv4
/// address needs its own socket. `v6only` is not read for IPv4.
fn dual_stack(fd: RawFd, addr: SocketAddr, v6only: V6Only) -> Result<(), String> {
    match (addr, v6only) {
        (SocketAddr::V4(_), V6Only::Yes | V6Only::No) | (SocketAddr::V6(_), V6Only::Yes) => Ok(()),
        (SocketAddr::V6(_), V6Only::No) => Err(format!(
            "systemd socket fd {fd} ({addr}) also accepts IPv4: set BindIPv6Only=ipv6-only"
        )),
    }
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
    match addr {
        SocketAddr::V4(_) => {}
        SocketAddr::V6(_) => {
            let v6only = ipv6_only(socket.as_fd())
                .map_err(|err| format!("systemd socket fd {fd}: {err}"))?;
            dual_stack(fd, addr, v6only)?;
        }
    }
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

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::net::SocketAddr;

    use super::{Announced, check_coverage, dual_stack, parse_env};
    use crate::sys::socket::V6Only;

    fn parse(pid: Option<&str>, fds: Option<&str>) -> Result<Announced, String> {
        parse_env(pid.map(OsStr::new), fds.map(OsStr::new), 42, 2)
    }

    #[test]
    fn the_activation_variables() {
        assert_eq!(parse(None, None), Ok(Announced::Nothing));
        assert_eq!(parse(Some("42"), Some("1")), Ok(Announced::Sockets(1)));
        assert_eq!(parse(Some("42"), Some("2")), Ok(Announced::Sockets(2)));
        let cases = [
            (
                None,
                Some("1"),
                "LISTEN_PID is not set, but the other one is",
            ),
            (
                Some("42"),
                None,
                "LISTEN_FDS is not set, but the other one is",
            ),
            (Some("x"), Some("1"), "LISTEN_PID is not a number: \"x\""),
            (Some("42"), Some("-1"), "LISTEN_FDS is not a number: \"-1\""),
            (Some("42"), Some(""), "LISTEN_FDS is not a number: \"\""),
            (
                Some("7"),
                Some("1"),
                "LISTEN_PID is 7, not this process (42)",
            ),
            (Some("42"), Some("0"), "LISTEN_FDS is 0"),
            (
                Some("42"),
                Some("3"),
                "systemd passed 3 sockets for 2 listen addresses",
            ),
        ];
        for (pid, fds, message) in cases {
            assert_eq!(parse(pid, fds), Err(message.to_owned()), "{pid:?} {fds:?}");
        }
    }

    #[test]
    fn every_address_once() {
        let a: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:2".parse().unwrap();
        assert_eq!(check_coverage(&[a, b], &[a, b]), Ok(()));
        assert_eq!(check_coverage(&[b, a], &[a, b]), Ok(()));
        assert_eq!(
            check_coverage(&[a, a], &[a, b]),
            Err("two systemd sockets are bound to 127.0.0.1:1".to_owned())
        );
        assert_eq!(
            check_coverage(&[a], &[a, b]),
            Err("listen 127.0.0.1:2 has no socket from systemd".to_owned())
        );
    }

    /// O4: an adopted IPv6 socket must be IPv6-only; IPv4 ones are not
    /// asked.
    #[test]
    fn an_ipv6_socket_must_be_ipv6_only() {
        let v6: SocketAddr = "[::]:80".parse().unwrap();
        let v4: SocketAddr = "0.0.0.0:80".parse().unwrap();
        assert_eq!(dual_stack(3, v6, V6Only::Yes), Ok(()));
        assert_eq!(
            dual_stack(3, v6, V6Only::No),
            Err("systemd socket fd 3 ([::]:80) also accepts IPv4: \
                 set BindIPv6Only=ipv6-only"
                .to_owned())
        );
        assert_eq!(dual_stack(4, v4, V6Only::No), Ok(()));
    }
}
