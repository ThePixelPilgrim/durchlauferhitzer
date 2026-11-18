// THROWAWAY spike: shared libc helpers, included with #[path] by every binary.
#![allow(dead_code)]

use std::io;
use std::os::fd::RawFd;

pub const SERVER_UID: u32 = 2001;
pub const HELPER_UID: u32 = 2002;
pub const KEYD_UID: u32 = 2003;
pub const KEYD_ABSTRACT: &[u8] = b"\0dlh-spike-keyd";
pub const HSPAWN_ABSTRACT: &[u8] = b"\0dlh-spike-hspawn";

pub fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn seqpacket_pair() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    cvt(unsafe {
        libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr())
    })?;
    Ok((fds[0], fds[1]))
}

pub fn set_passcred(fd: RawFd) -> io::Result<()> {
    let one: libc::c_int = 1;
    cvt(unsafe {
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_PASSCRED, (&one as *const libc::c_int).cast(), 4)
    })?;
    Ok(())
}

pub fn peercred(fd: RawFd) -> io::Result<libc::ucred> {
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    cvt(unsafe {
        libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut c as *mut libc::ucred).cast(), &mut len)
    })?;
    Ok(c)
}

/// sendmsg with optional fds (SCM_RIGHTS).
pub fn send_fds(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    let mut cbuf = [0u64; 16];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let space = unsafe { libc::CMSG_SPACE((fds.len() * 4) as u32) } as usize;
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast::<RawFd>(), fds.len());
        }
    }
    loop {
        let r = unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            return Err(e);
        }
        return Ok(r as usize);
    }
}

pub struct Received {
    pub len: usize,
    pub fds: Vec<RawFd>,
    pub cred: Option<libc::ucred>,
}

/// recvmsg collecting SCM_RIGHTS fds (made CLOEXEC) and SCM_CREDENTIALS.
pub fn recv_fds(sock: RawFd, buf: &mut [u8]) -> io::Result<Received> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; 32];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    let r = loop {
        let r = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            return Err(e);
        }
        break r as usize;
    };
    let mut out = Received { len: r, fds: Vec::new(), cred: None };
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let n = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / 4;
                let p = libc::CMSG_DATA(c).cast::<RawFd>();
                for i in 0..n { out.fds.push(*p.add(i)); }
            } else if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_CREDENTIALS {
                out.cred = Some(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::ucred>()));
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok(out)
}

pub fn abstract_addr(name: &[u8]) -> (libc::sockaddr_un, libc::socklen_t) {
    let mut a: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    a.sun_family = libc::AF_UNIX as u16;
    for (i, b) in name.iter().enumerate() { a.sun_path[i] = *b as libc::c_char; }
    (a, (2 + name.len()) as libc::socklen_t)
}

pub fn listen_abstract(name: &[u8]) -> io::Result<RawFd> {
    let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) })?;
    let (a, l) = abstract_addr(name);
    cvt(unsafe { libc::bind(fd, (&a as *const libc::sockaddr_un).cast(), l) })?;
    cvt(unsafe { libc::listen(fd, 1024) })?;
    Ok(fd)
}

pub fn connect_abstract(name: &[u8]) -> io::Result<RawFd> {
    let fd = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) })?;
    let (a, l) = abstract_addr(name);
    if let Err(e) = cvt(unsafe { libc::connect(fd, (&a as *const libc::sockaddr_un).cast(), l) }) {
        unsafe { libc::close(fd) };
        return Err(e);
    }
    Ok(fd)
}

/// Drop to uid/gid with no supplementary groups.
pub fn drop_to(uid: u32) -> io::Result<()> {
    cvt(unsafe { libc::setgroups(0, std::ptr::null()) })?;
    cvt(unsafe { libc::setresgid(uid, uid, uid) })?;
    cvt(unsafe { libc::setresuid(uid, uid, uid) })?;
    Ok(())
}

pub fn read_full(fd: RawFd, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let r = unsafe { libc::read(fd, buf[done..].as_mut_ptr().cast(), buf.len() - done) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            return Err(e);
        }
        if r == 0 { return Err(io::ErrorKind::UnexpectedEof.into()); }
        done += r as usize;
    }
    Ok(())
}

pub fn write_full(fd: RawFd, buf: &[u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let r = unsafe { libc::send(fd, buf[done..].as_ptr().cast(), buf.len() - done, libc::MSG_NOSIGNAL) };
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) { continue; }
            return Err(e);
        }
        done += r as usize;
    }
    Ok(())
}

pub fn maxrss_kib() -> i64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_maxrss
}

// ---------------------------------------------------------------- hand-over wire format
// Helper -> server: fixed layout, little endian.
//   0  [u8;16]  helper version string (NUL padded)
//  16  u16      cipher suite (0x1303 chacha, 0x1301 aes128gcm)
//  18  u64      tx seq (server->client direction)
//  26  u64      rx seq
//  34  [u8;32]  client application traffic secret 0 (from KeyLog)
//  66  [u8;32]  server application traffic secret 0 (from KeyLog)
//  98  [u8;32]  tx key (from dangerous_extract_secrets), 16 or 32 used
// 130  [u8;12]  tx iv
// 142  [u8;32]  rx key
// 174  [u8;12]  rx iv
// 186  u64*4    timings: main_ns, config_ns, inbox_ns, handshake_done_ns
// 218  i64      max rss KiB
pub const HANDOVER_LEN: usize = 226;
