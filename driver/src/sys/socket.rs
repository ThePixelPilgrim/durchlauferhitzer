//! Non-blocking TCP sockets: listen, accept, connect, read, write.

use std::io;
use std::mem::{size_of, zeroed};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use super::fd::{Io, cvt, cvt_retry, errno_is, transfer};

/// How a non-blocking connect started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStart {
    /// `connect` succeeded at once.
    Connected,
    /// `EINPROGRESS` (or `EINTR`, after which the connect also goes on in
    /// the background): wait for `EPOLLOUT`, then read [`take_error`].
    InProgress,
}

/// A socket whose connect has been started.
#[derive(Debug)]
pub struct Connecting {
    pub fd: OwnedFd,
    /// What `connect` itself returned; an `Err` is a connect that failed at
    /// once (the socket is still returned, for the caller to close).
    pub immediate: io::Result<ConnectStart>,
}

/// Whether a descriptor is a listening `AF_INET`/`AF_INET6` stream socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listening {
    Yes,
    No,
}

/// A new non-blocking, close-on-exec TCP socket for `addr`'s family.
fn tcp_socket(addr: &SocketAddr) -> io::Result<OwnedFd> {
    let family = match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };
    let kind = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    // SAFETY: `socket` takes no pointers.
    let fd = cvt(unsafe { libc::socket(family, kind, 0) })?;
    // SAFETY: `fd` was just created and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Binds a non-blocking listening socket with `SO_REUSEADDR` (and not
/// `SO_REUSEPORT`). An IPv6 listener sets `IPV6_V6ONLY`, so it accepts
/// IPv6 only, whatever the host's default (FR-CFG-10, A6).
pub fn tcp_listen(addr: SocketAddr, backlog: u32) -> io::Result<OwnedFd> {
    let fd = tcp_socket(&addr)?;
    set_int_option(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
    match addr {
        SocketAddr::V4(_) => {}
        SocketAddr::V6(_) => {
            set_int_option(fd.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 1)?;
        }
    }
    let (storage, len) = to_sockaddr(&addr);
    // SAFETY: `storage` holds a valid socket address of `len` bytes.
    cvt(unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&raw const storage).cast::<libc::sockaddr>(),
            len,
        )
    })?;
    let backlog = libc::c_int::try_from(backlog).unwrap_or(libc::c_int::MAX);
    // SAFETY: `listen` takes no pointers.
    cvt(unsafe { libc::listen(fd.as_raw_fd(), backlog) })?;
    Ok(fd)
}

/// Accepts one pending connection as a non-blocking, close-on-exec socket
/// (`accept4`); `None` when none is pending (`EAGAIN`).
pub fn accept_nonblocking(listener: BorrowedFd<'_>) -> io::Result<Option<(OwnedFd, SocketAddr)>> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { zeroed() };
    let mut len = socklen::<libc::sockaddr_storage>();
    let flags = libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
    let ret = cvt_retry(|| {
        // SAFETY: `storage` is writable for `len` bytes, and `len` is a
        // valid in/out length.
        unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                (&raw mut storage).cast::<libc::sockaddr>(),
                &mut len,
                flags,
            )
        }
    });
    let fd = match ret {
        Ok(fd) => fd,
        Err(err) if errno_is(&err, libc::EAGAIN) => return Ok(None),
        Err(err) => return Err(err),
    };
    // SAFETY: `fd` was just created by `accept4` and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let peer = from_sockaddr(&storage)?;
    Ok(Some((fd, peer)))
}

/// Creates a non-blocking socket and starts connecting it to `addr`. An
/// `Err` here is the `socket` call failing (`EMFILE`, ...); a connect that
/// fails at once is in [`Connecting::immediate`].
pub fn connect_nonblocking(addr: SocketAddr) -> io::Result<Connecting> {
    let fd = tcp_socket(&addr)?;
    let (storage, len) = to_sockaddr(&addr);
    // SAFETY: `storage` holds a valid socket address of `len` bytes.
    let ret = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const storage).cast::<libc::sockaddr>(),
            len,
        )
    };
    let immediate = match cvt(ret) {
        Ok(_) => Ok(ConnectStart::Connected),
        Err(err) if errno_is(&err, libc::EINPROGRESS) || errno_is(&err, libc::EINTR) => {
            Ok(ConnectStart::InProgress)
        }
        Err(err) => Err(err),
    };
    Ok(Connecting { fd, immediate })
}

/// Reads and clears the socket's pending error (`SO_ERROR`).
pub fn take_error(fd: BorrowedFd<'_>) -> io::Result<Option<io::Error>> {
    let errno = get_int_option(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_ERROR)?;
    Ok((errno != 0).then(|| io::Error::from_raw_os_error(errno)))
}

/// Turns on `TCP_NODELAY` (NFR-PERF-04).
pub fn set_nodelay(fd: BorrowedFd<'_>) -> io::Result<()> {
    set_int_option(fd.as_raw_fd(), libc::IPPROTO_TCP, libc::TCP_NODELAY, 1)
}

/// A socket's buffer sizes as the kernel reports them (`SO_SNDBUF`,
/// `SO_RCVBUF`). The driver never sets them (NFR-PERF-04); the tests read
/// them to check that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferSizes {
    pub send: libc::c_int,
    pub receive: libc::c_int,
}

/// Reads `SO_SNDBUF` and `SO_RCVBUF`.
pub fn buffer_sizes(fd: BorrowedFd<'_>) -> io::Result<BufferSizes> {
    let raw = fd.as_raw_fd();
    Ok(BufferSizes {
        send: get_int_option(raw, libc::SOL_SOCKET, libc::SO_SNDBUF)?,
        receive: get_int_option(raw, libc::SOL_SOCKET, libc::SO_RCVBUF)?,
    })
}

/// Reads into `buf`. An empty `buf` returns `Done(0)` without a system
/// call (a 0 from `read` would look like the end of stream).
pub fn read(fd: BorrowedFd<'_>, buf: &mut [u8]) -> Io {
    if buf.is_empty() {
        return Io::Done(0);
    }
    transfer(|| {
        // SAFETY: `buf` is writable for `buf.len()` bytes.
        unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) }
    })
}

/// Writes from `buf` with `send(MSG_NOSIGNAL)`, so a closed peer is an
/// `EPIPE` error, never a `SIGPIPE`. An empty `buf` returns `Done(0)`
/// without a system call.
pub fn write(fd: BorrowedFd<'_>, buf: &[u8]) -> Io {
    if buf.is_empty() {
        return Io::Done(0);
    }
    transfer(|| {
        // SAFETY: `buf` is readable for `buf.len()` bytes.
        unsafe {
            libc::send(
                fd.as_raw_fd(),
                buf.as_ptr().cast(),
                buf.len(),
                libc::MSG_NOSIGNAL,
            )
        }
    })
}

/// Shuts down the sending direction (`SHUT_WR`).
pub fn shutdown_write(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: `shutdown` takes no pointers.
    cvt(unsafe { libc::shutdown(fd.as_raw_fd(), libc::SHUT_WR) })?;
    Ok(())
}

/// The address the socket is bound to (`getsockname`).
pub fn local_addr(fd: BorrowedFd<'_>) -> io::Result<SocketAddr> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { zeroed() };
    let mut len = socklen::<libc::sockaddr_storage>();
    // SAFETY: `storage` is writable for `len` bytes, and `len` is a valid
    // in/out length.
    cvt(unsafe {
        libc::getsockname(
            fd.as_raw_fd(),
            (&raw mut storage).cast::<libc::sockaddr>(),
            &mut len,
        )
    })?;
    from_sockaddr(&storage)
}

/// Whether `fd` is a listening `AF_INET`/`AF_INET6` TCP socket
/// (`SO_DOMAIN`, `SO_TYPE`, `SO_PROTOCOL`, `SO_ACCEPTCONN`). A descriptor that is not a
/// socket at all (`ENOTSOCK`) is `No`.
pub fn is_listening_stream(fd: BorrowedFd<'_>) -> io::Result<Listening> {
    let raw = fd.as_raw_fd();
    let domain = match get_int_option(raw, libc::SOL_SOCKET, libc::SO_DOMAIN) {
        Ok(domain) => domain,
        Err(err) if errno_is(&err, libc::ENOTSOCK) => return Ok(Listening::No),
        Err(err) => return Err(err),
    };
    let kind = get_int_option(raw, libc::SOL_SOCKET, libc::SO_TYPE)?;
    let protocol = get_int_option(raw, libc::SOL_SOCKET, libc::SO_PROTOCOL)?;
    let accepting = get_int_option(raw, libc::SOL_SOCKET, libc::SO_ACCEPTCONN)?;
    Ok(classify(domain, kind, protocol, accepting))
}

/// A listening TCP socket: `AF_INET`/`AF_INET6`, `SOCK_STREAM`,
/// `IPPROTO_TCP` (an SCTP stream socket is not one) and accepting.
fn classify(
    domain: libc::c_int,
    kind: libc::c_int,
    protocol: libc::c_int,
    accepting: libc::c_int,
) -> Listening {
    let inet = domain == libc::AF_INET || domain == libc::AF_INET6;
    if inet && kind == libc::SOCK_STREAM && protocol == libc::IPPROTO_TCP && accepting != 0 {
        Listening::Yes
    } else {
        Listening::No
    }
}

/// Whether an IPv6 socket accepts IPv6 only (`IPV6_V6ONLY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum V6Only {
    Yes,
    /// It accepts IPv4 too (as IPv4-mapped addresses).
    No,
}

/// Reads `IPV6_V6ONLY` of an `AF_INET6` socket.
pub fn ipv6_only(fd: BorrowedFd<'_>) -> io::Result<V6Only> {
    match get_int_option(fd.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)? {
        0 => Ok(V6Only::No),
        _ => Ok(V6Only::Yes),
    }
}

fn socklen<T>() -> libc::socklen_t {
    // The socket address structs are a few dozen bytes.
    libc::socklen_t::try_from(size_of::<T>()).unwrap_or(libc::socklen_t::MAX)
}

fn get_int_option(
    fd: libc::c_int,
    level: libc::c_int,
    name: libc::c_int,
) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = socklen::<libc::c_int>();
    // SAFETY: `value` is writable for `len` bytes, and `len` is a valid
    // in/out length.
    cvt(unsafe {
        libc::getsockopt(
            fd,
            level,
            name,
            (&raw mut value).cast::<libc::c_void>(),
            &mut len,
        )
    })?;
    Ok(value)
}

fn set_int_option(
    fd: libc::c_int,
    level: libc::c_int,
    name: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    // SAFETY: `value` is readable for the given length.
    cvt(unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            (&raw const value).cast::<libc::c_void>(),
            socklen::<libc::c_int>(),
        )
    })?;
    Ok(())
}

/// Encodes a socket address for `bind`/`connect`.
fn to_sockaddr(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: an all-zero `sockaddr_in` is a valid value.
            let mut sin: libc::sockaddr_in = unsafe { zeroed() };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr = libc::in_addr {
                s_addr: u32::from_ne_bytes(v4.ip().octets()),
            };
            // SAFETY: `sockaddr_storage` is large and aligned enough for any
            // socket address, so writing a `sockaddr_in` at its start is in
            // bounds and aligned.
            unsafe { (&raw mut storage).cast::<libc::sockaddr_in>().write(sin) };
            (storage, socklen::<libc::sockaddr_in>())
        }
        SocketAddr::V6(v6) => {
            // SAFETY: an all-zero `sockaddr_in6` is a valid value.
            let mut sin6: libc::sockaddr_in6 = unsafe { zeroed() };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            // Raw, as std does: `flowinfo` is the C field as it is.
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_addr = libc::in6_addr {
                s6_addr: v6.ip().octets(),
            };
            sin6.sin6_scope_id = v6.scope_id();
            // SAFETY: as above, for a `sockaddr_in6`.
            unsafe { (&raw mut storage).cast::<libc::sockaddr_in6>().write(sin6) };
            (storage, socklen::<libc::sockaddr_in6>())
        }
    }
}

/// Decodes a socket address the kernel filled in.
fn from_sockaddr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    let family = libc::c_int::from(storage.ss_family);
    if family == libc::AF_INET {
        // SAFETY: the family says the storage holds a `sockaddr_in`; the
        // storage is large and aligned enough to read one.
        let sin = unsafe {
            (storage as *const libc::sockaddr_storage)
                .cast::<libc::sockaddr_in>()
                .read()
        };
        let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
        Ok(SocketAddr::V4(SocketAddrV4::new(
            ip,
            u16::from_be(sin.sin_port),
        )))
    } else if family == libc::AF_INET6 {
        // SAFETY: as above, for a `sockaddr_in6`.
        let sin6 = unsafe {
            (storage as *const libc::sockaddr_storage)
                .cast::<libc::sockaddr_in6>()
                .read()
        };
        Ok(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(sin6.sin6_addr.s6_addr),
            u16::from_be(sin6.sin6_port),
            sin6.sin6_flowinfo,
            sin6.sin6_scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not an IP socket address (family {family})"),
        ))
    }
}

/// The IPv6 tests' skip rule, shared with `tests/sys.rs`.
#[cfg(test)]
#[path = "../../tests/support/ipv6.rs"]
mod ipv6;

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6};

    use super::{from_sockaddr, get_int_option, tcp_listen, to_sockaddr};

    use super::ipv6;

    #[test]
    fn ipv6_flowinfo_is_passed_raw_like_std() {
        // std passes `sin6_flowinfo` through unconverted (its docs say
        // `flowinfo` corresponds to the C field), and so do we, so our
        // addresses compare equal to std's (A7).
        let addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 80, 0x0001_2345, 7));
        let (storage, _) = to_sockaddr(&addr);
        // SAFETY: `to_sockaddr` wrote a `sockaddr_in6` at the start of the
        // storage, which is large and aligned enough to read one.
        let sin6 = unsafe { (&raw const storage).cast::<libc::sockaddr_in6>().read() };
        assert_eq!(sin6.sin6_flowinfo, 0x0001_2345);
        assert_eq!(sin6.sin6_scope_id, 7);
        assert_eq!(from_sockaddr(&storage).unwrap(), addr);
    }

    #[test]
    fn only_listening_tcp_sockets_count() {
        use super::{Listening, classify};
        let tcp = libc::IPPROTO_TCP;
        let yes = [
            (libc::AF_INET, libc::SOCK_STREAM, tcp, 1),
            (libc::AF_INET6, libc::SOCK_STREAM, tcp, 1),
        ];
        for (domain, kind, protocol, accepting) in yes {
            assert_eq!(classify(domain, kind, protocol, accepting), Listening::Yes);
        }
        let no = [
            // SCTP's one-to-one style is a stream socket too (M4).
            (libc::AF_INET, libc::SOCK_STREAM, libc::IPPROTO_SCTP, 1),
            (libc::AF_INET6, libc::SOCK_STREAM, libc::IPPROTO_SCTP, 1),
            (libc::AF_INET, libc::SOCK_STREAM, tcp, 0),
            (libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_UDP, 0),
            (libc::AF_UNIX, libc::SOCK_STREAM, 0, 1),
        ];
        for (domain, kind, protocol, accepting) in no {
            assert_eq!(
                classify(domain, kind, protocol, accepting),
                Listening::No,
                "{domain} {kind} {protocol} {accepting}"
            );
        }
    }

    #[test]
    fn listeners_set_reuseaddr_and_not_reuseport() {
        let listener = tcp_listen("127.0.0.1:0".parse().unwrap(), 16).unwrap();
        let fd = listener.as_raw_fd();
        let reuseaddr = get_int_option(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR).unwrap();
        let reuseport = get_int_option(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT).unwrap();
        assert_eq!((reuseaddr, reuseport), (1, 0));
    }

    #[test]
    fn ipv6_listeners_are_ipv6_only() {
        let Some(listener) = ipv6::or_skip(tcp_listen("[::1]:0".parse().unwrap(), 16)) else {
            return;
        };
        let v6only =
            get_int_option(listener.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_V6ONLY).unwrap();
        assert_eq!(v6only, 1);
    }
}
