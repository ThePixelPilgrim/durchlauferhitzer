//! Throwaway kTLS spike. Probes the kernel TLS record layer and prints one
//! `PROBE <id> <name>: OK|FAIL|SKIP <detail>` line per probe. Exit 0 always.
#![allow(clippy::all)]

use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aes_gcm::{Aes128Gcm, Aes256Gcm};
use chacha20poly1305::ChaCha20Poly1305;
use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};

const SOL_TLS: i32 = 282;
const TCP_ULP: i32 = 31;
const TLS_TX: i32 = 1;
const TLS_RX: i32 = 2;
const TLS_TX_ZEROCOPY_RO: i32 = 3;
const TLS_RX_EXPECT_NO_PAD: i32 = 4;
const TLS_SET_RECORD_TYPE: i32 = 1;
const TLS_GET_RECORD_TYPE: i32 = 2;
const V12: u16 = 0x0303;
const V13: u16 = 0x0304;
const KEY_UPDATE: [u8; 5] = [24, 0, 0, 1, 0];

type R = Result<String, String>;

static ULP_OK: AtomicBool = AtomicBool::new(false);
static RESULTS: Mutex<Vec<(String, String, &'static str)>> = Mutex::new(Vec::new());

// ---------------------------------------------------------------- helpers

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn ename(e: i32) -> &'static str {
    match e {
        libc::EPERM => "EPERM",
        libc::ENOENT => "ENOENT",
        libc::EINTR => "EINTR",
        libc::EIO => "EIO",
        libc::EBADF => "EBADF",
        libc::EAGAIN => "EAGAIN",
        libc::ENOMEM => "ENOMEM",
        libc::EACCES => "EACCES",
        libc::EFAULT => "EFAULT",
        libc::EBUSY => "EBUSY",
        libc::EEXIST => "EEXIST",
        libc::EINVAL => "EINVAL",
        libc::ENOSPC => "ENOSPC",
        libc::ESPIPE => "ESPIPE",
        libc::EPIPE => "EPIPE",
        libc::ENOPROTOOPT => "ENOPROTOOPT",
        libc::EPROTONOSUPPORT => "EPROTONOSUPPORT",
        libc::EOPNOTSUPP => "EOPNOTSUPP",
        libc::ENOTCONN => "ENOTCONN",
        libc::ECONNRESET => "ECONNRESET",
        libc::ECONNABORTED => "ECONNABORTED",
        libc::EMSGSIZE => "EMSGSIZE",
        libc::EBADMSG => "EBADMSG",
        libc::EKEYEXPIRED => "EKEYEXPIRED",
        libc::ENOBUFS => "ENOBUFS",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::EPROTO => "EPROTO",
        libc::ENOTSOCK => "ENOTSOCK",
        libc::ENOKEY => "ENOKEY",
        libc::EOVERFLOW => "EOVERFLOW",
        libc::ESHUTDOWN => "ESHUTDOWN",
        libc::EISCONN => "EISCONN",
        libc::ENOSYS => "ENOSYS",
        libc::ERANGE => "ERANGE",
        _ => "E?",
    }
}

fn syserr(call: &str) -> String {
    let e = errno();
    let note = if e == libc::EAGAIN { " (socket timeout)" } else { "" };
    format!("{call} -> {}({e}){note}", ename(e))
}

fn need_ulp() -> Result<(), String> {
    if ULP_OK.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err("SKIP TCP_ULP \"tls\" unavailable in this run (see probe 1.1)".into())
    }
}

fn probe(id: &str, name: &str, f: impl FnOnce() -> R) {
    let r = std::panic::catch_unwind(AssertUnwindSafe(f));
    let (st, d): (&'static str, String) = match r {
        Ok(Ok(d)) => ("OK", d),
        Ok(Err(e)) if e.starts_with("SKIP ") => ("SKIP", e[5..].to_string()),
        Ok(Err(e)) => ("FAIL", e),
        Err(p) => {
            let m = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default();
            ("FAIL", format!("probe panicked: {m}"))
        }
    };
    println!("PROBE {id} {name}: {st} {d}");
    RESULTS.lock().unwrap().push((id.to_string(), name.to_string(), st));
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    let mut x: u32 = seed.wrapping_mul(2654435761).wrapping_add(12345) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

fn first_diff(a: &[u8], b: &[u8]) -> String {
    if a.len() != b.len() {
        return format!("length {} != {}", a.len(), b.len());
    }
    let i = a.iter().zip(b).position(|(x, y)| x != y).unwrap_or(0);
    format!("first difference at byte {i}")
}

fn pair() -> Result<(TcpStream, TcpStream), String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("bind: {e}"))?;
    let a = TcpStream::connect(l.local_addr().unwrap()).map_err(|e| format!("connect: {e}"))?;
    let (b, _) = l.accept().map_err(|e| format!("accept: {e}"))?;
    for s in [&a, &b] {
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(10))).unwrap();
    }
    Ok((a, b))
}

fn ulp(fd: RawFd) -> Result<(), String> {
    let n = b"tls";
    let r = unsafe {
        libc::setsockopt(fd, libc::SOL_TCP, TCP_ULP, n.as_ptr() as *const _, n.len() as u32)
    };
    if r < 0 {
        Err(syserr("setsockopt(SOL_TCP,TCP_ULP,\"tls\")"))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Cipher {
    Chacha,
    Aes128,
    Aes256,
}

impl Cipher {
    fn id(self) -> u16 {
        match self {
            Cipher::Aes128 => 51,
            Cipher::Aes256 => 52,
            Cipher::Chacha => 54,
        }
    }
    fn key_len(self) -> usize {
        match self {
            Cipher::Aes128 => 16,
            _ => 32,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Cipher::Aes128 => "aes128gcm",
            Cipher::Aes256 => "aes256gcm",
            Cipher::Chacha => "chacha20poly1305",
        }
    }
}

#[derive(Clone)]
struct Keys {
    c: Cipher,
    key: Vec<u8>,
    iv: [u8; 12],
}

fn keys(c: Cipher, seed: u8) -> Keys {
    let key = (0..c.key_len()).map(|i| seed.wrapping_add((i as u8).wrapping_mul(7))).collect();
    let mut iv = [0u8; 12];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = (seed ^ 0xa5).wrapping_add((i as u8).wrapping_mul(13));
    }
    Keys { c, key, iv }
}

/// struct tls12_crypto_info_*: info, iv, key, salt, rec_seq (all u8 arrays).
fn cinfo(ver: u16, k: &Keys, seq: u64) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend(ver.to_ne_bytes());
    v.extend(k.c.id().to_ne_bytes());
    match k.c {
        Cipher::Chacha => {
            v.extend(&k.iv);
            v.extend(&k.key);
        }
        _ => {
            v.extend(&k.iv[4..]);
            v.extend(&k.key);
            v.extend(&k.iv[..4]);
        }
    }
    v.extend(seq.to_be_bytes());
    v
}

fn set_raw(fd: RawFd, dir: i32, info: &[u8]) -> Result<(), String> {
    let r = unsafe {
        libc::setsockopt(fd, SOL_TLS, dir, info.as_ptr() as *const _, info.len() as u32)
    };
    if r < 0 {
        Err(syserr(if dir == TLS_TX {
            "setsockopt(SOL_TLS,TLS_TX)"
        } else {
            "setsockopt(SOL_TLS,TLS_RX)"
        }))
    } else {
        Ok(())
    }
}

fn install(fd: RawFd, dir: i32, ver: u16, k: &Keys, seq: u64) -> Result<(), String> {
    set_raw(fd, dir, &cinfo(ver, k, seq))
}

fn set_int(fd: RawFd, level: i32, opt: i32, val: i32, what: &str) -> Result<(), String> {
    let r = unsafe {
        libc::setsockopt(fd, level, opt, &val as *const i32 as *const _, 4)
    };
    if r < 0 { Err(syserr(what)) } else { Ok(()) }
}

fn get_int(fd: RawFd, level: i32, opt: i32, what: &str) -> Result<i32, String> {
    let mut v: i32 = -1;
    let mut l: libc::socklen_t = 4;
    let r = unsafe { libc::getsockopt(fd, level, opt, &mut v as *mut i32 as *mut _, &mut l) };
    if r < 0 { Err(syserr(what)) } else { Ok(v) }
}

fn nonce(k: &Keys, seq: u64) -> [u8; 12] {
    let mut n = k.iv;
    for (i, b) in seq.to_be_bytes().iter().enumerate() {
        n[4 + i] ^= b;
    }
    n
}

fn aead_seal(k: &Keys, n: &[u8; 12], aad: &[u8], msg: &[u8]) -> Vec<u8> {
    let nn = GenericArray::from_slice(n);
    let p = Payload { msg, aad };
    match k.c {
        Cipher::Chacha => ChaCha20Poly1305::new_from_slice(&k.key).unwrap().encrypt(nn, p),
        Cipher::Aes128 => Aes128Gcm::new_from_slice(&k.key).unwrap().encrypt(nn, p),
        Cipher::Aes256 => Aes256Gcm::new_from_slice(&k.key).unwrap().encrypt(nn, p),
    }
    .unwrap()
}

fn aead_open(k: &Keys, n: &[u8; 12], aad: &[u8], ct: &[u8]) -> Result<Vec<u8>, String> {
    let nn = GenericArray::from_slice(n);
    let p = Payload { msg: ct, aad };
    match k.c {
        Cipher::Chacha => ChaCha20Poly1305::new_from_slice(&k.key).unwrap().decrypt(nn, p),
        Cipher::Aes128 => Aes128Gcm::new_from_slice(&k.key).unwrap().decrypt(nn, p),
        Cipher::Aes256 => Aes256Gcm::new_from_slice(&k.key).unwrap().decrypt(nn, p),
    }
    .map_err(|_| "userspace AEAD decrypt failed (tag mismatch)".to_string())
}

/// TLS 1.3 record built in userspace.
fn seal13(k: &Keys, seq: u64, ctype: u8, data: &[u8], pad: usize) -> Vec<u8> {
    let mut inner = data.to_vec();
    inner.push(ctype);
    inner.extend(std::iter::repeat(0).take(pad));
    let len = inner.len() + 16;
    let hdr = [23u8, 3, 3, (len >> 8) as u8, len as u8];
    let mut out = hdr.to_vec();
    out.extend(aead_seal(k, &nonce(k, seq), &hdr, &inner));
    out
}

/// Returns (inner content type, plaintext, padding length).
fn open13(k: &Keys, seq: u64, rec: &[u8]) -> Result<(u8, Vec<u8>, usize), String> {
    if rec.len() < 5 + 17 {
        return Err(format!("record too short ({} bytes)", rec.len()));
    }
    let mut pt = aead_open(k, &nonce(k, seq), &rec[..5], &rec[5..])?;
    let before = pt.len();
    while pt.last() == Some(&0) {
        pt.pop();
    }
    let pad = before - pt.len();
    let ty = pt.pop().ok_or("inner plaintext all zero")?;
    Ok((ty, pt, pad))
}

fn write_all(fd: RawFd, data: &[u8]) -> Result<(), String> {
    let mut off = 0;
    while off < data.len() {
        let r = unsafe {
            libc::send(fd, data[off..].as_ptr() as *const _, data.len() - off, libc::MSG_NOSIGNAL)
        };
        if r < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return Err(format!("{} after {off}/{} bytes", syserr("send"), data.len()));
        }
        off += r as usize;
    }
    Ok(())
}

fn read_exact(fd: RawFd, n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let r = unsafe { libc::recv(fd, buf[got..].as_mut_ptr() as *mut _, n - got, 0) };
        if r < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return Err(format!("{} after {got}/{n} bytes", syserr("recv")));
        }
        if r == 0 {
            return Err(format!("recv -> EOF after {got}/{n} bytes"));
        }
        got += r as usize;
    }
    Ok(buf)
}

/// One recv; Ok(n) or Err(errno string).
fn recv1(fd: RawFd, cap: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; cap];
    let r = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut _, cap, 0) };
    if r < 0 {
        return Err(syserr("recv"));
    }
    buf.truncate(r as usize);
    Ok(buf)
}

fn read_record(fd: RawFd) -> Result<Vec<u8>, String> {
    let mut h = read_exact(fd, 5)?;
    let len = u16::from_be_bytes([h[3], h[4]]) as usize;
    let b = read_exact(fd, len)?;
    h.extend(b);
    Ok(h)
}

/// sendmsg with a SOL_TLS/TLS_SET_RECORD_TYPE cmsg.
fn send_rec(fd: RawFd, ty: u8, data: &[u8]) -> Result<usize, String> {
    unsafe {
        let mut cbuf = [0u64; 4];
        let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = libc::CMSG_SPACE(1) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = SOL_TLS;
        (*c).cmsg_type = TLS_SET_RECORD_TYPE;
        (*c).cmsg_len = libc::CMSG_LEN(1) as _;
        *libc::CMSG_DATA(c) = ty;
        let r = libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL);
        if r < 0 {
            Err(syserr(&format!("sendmsg(cmsg TLS_SET_RECORD_TYPE={ty})")))
        } else {
            Ok(r as usize)
        }
    }
}

/// recvmsg with a control buffer: (data, record type from cmsg, msg_flags).
fn recv_rec(fd: RawFd, cap: usize) -> Result<(Vec<u8>, Option<u8>, i32), String> {
    unsafe {
        let mut buf = vec![0u8; cap];
        let mut cbuf = [0u64; 8];
        let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut _, iov_len: cap };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
        let r = libc::recvmsg(fd, &mut msg, 0);
        if r < 0 {
            return Err(syserr("recvmsg(with cmsg buffer)"));
        }
        let mut ty = None;
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == SOL_TLS && (*c).cmsg_type == TLS_GET_RECORD_TYPE {
            ty = Some(*libc::CMSG_DATA(c));
        }
        buf.truncate(r as usize);
        Ok((buf, ty, msg.msg_flags))
    }
}

fn sleep_ms(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

fn so_error(fd: RawFd) -> String {
    match get_int(fd, libc::SOL_SOCKET, libc::SO_ERROR, "getsockopt(SO_ERROR)") {
        Ok(0) => "SO_ERROR=0".into(),
        Ok(e) => format!("SO_ERROR={}({e})", ename(e)),
        Err(e) => e,
    }
}

/// kTLS pair: A has TX, B has RX, same keys.
fn tls_pair(ver: u16, k: &Keys) -> Result<(TcpStream, TcpStream), String> {
    need_ulp()?;
    let (a, b) = pair()?;
    ulp(a.as_raw_fd())?;
    ulp(b.as_raw_fd())?;
    install(a.as_raw_fd(), TLS_TX, ver, k, 0)?;
    install(b.as_raw_fd(), TLS_RX, ver, k, 0)?;
    Ok((a, b))
}

fn xfer(a: RawFd, b: RawFd, n: usize, seed: u32) -> R {
    let data = pattern(n, seed);
    let d2 = data.clone();
    let h = thread::spawn(move || write_all(a, &d2));
    let got = read_exact(b, n);
    let w = h.join().map_err(|_| "sender thread panicked".to_string())?;
    let got = match (got, w) {
        (Ok(g), Ok(())) => g,
        (Err(e), Ok(())) => return Err(format!("reader: {e}")),
        (Ok(_), Err(e)) => return Err(format!("sender: {e}")),
        (Err(e), Err(f)) => return Err(format!("reader: {e}; sender: {f}")),
    };
    if got != data {
        return Err(format!("data mismatch: {}", first_diff(&got, &data)));
    }
    Ok(format!("{n} bytes round-tripped"))
}

fn tmpfile(data: &[u8]) -> Result<std::fs::File, String> {
    use std::io::Write;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let p = std::env::temp_dir().join(format!(
        "ktls-spike-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let mut f = std::fs::File::create(&p).map_err(|e| format!("create tmp: {e}"))?;
    f.write_all(data).map_err(|e| format!("write tmp: {e}"))?;
    drop(f);
    let f = std::fs::File::open(&p).map_err(|e| format!("open tmp: {e}"))?;
    let _ = std::fs::remove_file(&p);
    Ok(f)
}

fn sendfile_all(sock: RawFd, file: RawFd, n: usize) -> Result<usize, String> {
    let mut off: libc::off_t = 0;
    let mut calls = 0;
    while (off as usize) < n {
        let r = unsafe { libc::sendfile(sock, file, &mut off, n - off as usize) };
        calls += 1;
        if r < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            return Err(format!("{} at offset {off}", syserr("sendfile")));
        }
        if r == 0 {
            return Err(format!("sendfile returned 0 at offset {off}"));
        }
    }
    Ok(calls)
}

/// Read records on a plain socket until `total` plaintext bytes; decrypt each.
fn read_records_plain(b: RawFd, k: &Keys, total: usize) -> Result<(Vec<u8>, Vec<usize>), String> {
    let mut out = Vec::new();
    let mut lens = Vec::new();
    let mut seq = 0u64;
    while out.len() < total {
        let rec = read_record(b)?;
        if rec[0] != 23 || rec[1] != 3 || rec[2] != 3 {
            return Err(format!("record {seq}: bad header {:02x?}", &rec[..5]));
        }
        let (ty, pt, pad) = open13(k, seq, &rec).map_err(|e| format!("record {seq}: {e}"))?;
        if ty != 23 || pad != 0 {
            return Err(format!("record {seq}: inner type {ty} pad {pad}"));
        }
        lens.push(pt.len());
        out.extend(pt);
        seq += 1;
    }
    Ok((out, lens))
}

fn summarize_lens(lens: &[usize]) -> String {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &l in lens {
        match runs.last_mut() {
            Some((v, c)) if *v == l => *c += 1,
            _ => runs.push((l, 1)),
        }
    }
    let s: Vec<String> = runs.iter().map(|(v, c)| if *c > 1 { format!("{c}x{v}") } else { v.to_string() }).collect();
    let mut j = s.join(",");
    if j.len() > 300 {
        j.truncate(300);
        j.push_str("...");
    }
    format!("{} records, plaintext lengths [{j}]", lens.len())
}

fn fork_child(f: impl FnOnce() -> i32) -> Result<libc::pid_t, String> {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(syserr("fork"));
    }
    if pid == 0 {
        let code = std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(99);
        let _ = std::io::stdout().flush();
        unsafe { libc::_exit(code) }
    }
    Ok(pid)
}

fn reap(pid: libc::pid_t) -> String {
    let start = Instant::now();
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WNOHANG) };
        if r == pid {
            return format!("child exit status {}", libc::WEXITSTATUS(st));
        }
        if start.elapsed() > Duration::from_secs(15) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            unsafe { libc::waitpid(pid, &mut st, 0) };
            return "child killed after 15s".into();
        }
        sleep_ms(20);
    }
}

fn socketpair() -> Result<(RawFd, RawFd), String> {
    let mut sv = [0; 2];
    let r = unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) };
    if r < 0 {
        return Err(syserr("socketpair"));
    }
    let tv = libc::timeval { tv_sec: 5, tv_usec: 0 };
    for fd in sv {
        unsafe {
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, &tv as *const _ as *const _, std::mem::size_of_val(&tv) as u32);
        }
    }
    Ok((sv[0], sv[1]))
}

fn send_fd(sock: RawFd, fd: RawFd) -> Result<(), String> {
    unsafe {
        let mut cbuf = [0u64; 4];
        let b = [b'F'];
        let mut iov = libc::iovec { iov_base: b.as_ptr() as *mut _, iov_len: 1 };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = libc::CMSG_SPACE(4) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::copy_nonoverlapping(&fd as *const i32 as *const u8, libc::CMSG_DATA(c), 4);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(syserr("sendmsg(SCM_RIGHTS)"));
        }
    }
    Ok(())
}

fn recv_fd(sock: RawFd) -> Result<RawFd, String> {
    unsafe {
        let mut cbuf = [0u64; 4];
        let mut b = [0u8];
        let mut iov = libc::iovec { iov_base: b.as_mut_ptr() as *mut _, iov_len: 1 };
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
        if libc::recvmsg(sock, &mut msg, 0) < 0 {
            return Err(syserr("recvmsg(SCM_RIGHTS)"));
        }
        let c = libc::CMSG_FIRSTHDR(&msg);
        if c.is_null() || (*c).cmsg_type != libc::SCM_RIGHTS {
            return Err("no SCM_RIGHTS cmsg".into());
        }
        let mut fd: i32 = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c), &mut fd as *mut i32 as *mut u8, 4);
        Ok(fd)
    }
}

fn read_tls_stat() -> Vec<(String, i64)> {
    std::fs::read_to_string("/proc/net/tls_stat")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect()
}

/// getsockopt(SOL_TLS, dir) with a full-size buffer; compare with installed info.
fn readback(fd: RawFd, dir: i32, ver: u16, k: &Keys, seq_expected: u64) -> R {
    let want = cinfo(ver, k, seq_expected);
    let mut buf = vec![0xeeu8; want.len()];
    let mut l = buf.len() as libc::socklen_t;
    let r = unsafe { libc::getsockopt(fd, SOL_TLS, dir, buf.as_mut_ptr() as *mut _, &mut l) };
    let dname = if dir == TLS_TX { "TLS_TX" } else { "TLS_RX" };
    if r < 0 {
        return Err(syserr(&format!("getsockopt(SOL_TLS,{dname},len={})", want.len())));
    }
    // locate key in the struct
    let key_off = match k.c {
        Cipher::Chacha => 4 + 12,
        _ => 4 + 8,
    };
    let key_got = &buf[key_off..key_off + k.key.len()];
    let key_state = if key_got == &k.key[..] {
        "KEY RETURNED IN CLEAR (identical to installed key)"
    } else if key_got.iter().all(|&b| b == 0) {
        "key field all zero"
    } else if key_got.iter().all(|&b| b == 0xee) {
        "key field untouched by kernel"
    } else {
        "key field differs from installed key"
    };
    let rest_same = buf[..key_off] == want[..key_off] && buf[key_off + k.key.len()..] == want[key_off + k.key.len()..];
    Ok(format!(
        "returned len={l}; {key_state}; version/cipher/iv/salt/rec_seq {} (rec_seq bytes {:02x?})",
        if rest_same { "match installed+expected seq" } else { "differ" },
        &buf[buf.len() - 8..]
    ))
}

// ---------------------------------------------------------------- probes

fn p1() {
    probe("1.1", "ulp-on-connected-client", || {
        let (a, _b) = pair()?;
        ulp(a.as_raw_fd())?;
        ULP_OK.store(true, Ordering::SeqCst);
        Ok("attached".into())
    });
    if !ULP_OK.load(Ordering::SeqCst) {
        // Look whether the module got loaded anyway (it should not for non-root).
        let loaded = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_available_ulp").unwrap_or_default();
        println!("INFO tcp_available_ulp after failed attach: {:?}", loaded.trim());
    }
    probe("1.2", "ulp-on-accepted", || {
        need_ulp()?;
        let (_a, b) = pair()?;
        ulp(b.as_raw_fd())?;
        Ok("attached".into())
    });
    probe("1.3", "ulp-on-listener(expect-reject)", || {
        need_ulp()?;
        let l = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        match ulp(l.as_raw_fd()) {
            Ok(()) => Err("UNEXPECTED: attach on listening socket succeeded".into()),
            Err(e) => Ok(format!("rejected: {e}")),
        }
    });
    probe("1.4", "ulp-on-unconnected(expect-reject)", || {
        need_ulp()?;
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        let s = unsafe { TcpStream::from_raw_fd(fd) };
        match ulp(s.as_raw_fd()) {
            Ok(()) => Err("UNEXPECTED: attach on unconnected socket succeeded".into()),
            Err(e) => Ok(format!("rejected: {e}")),
        }
    });
    probe("1.5", "ulp-getsockopt-readback", || {
        need_ulp()?;
        let (a, _b) = pair()?;
        ulp(a.as_raw_fd())?;
        let mut buf = [0u8; 16];
        let mut l: libc::socklen_t = 16;
        let r = unsafe { libc::getsockopt(a.as_raw_fd(), libc::SOL_TCP, TCP_ULP, buf.as_mut_ptr() as *mut _, &mut l) };
        if r < 0 {
            return Err(syserr("getsockopt(TCP_ULP)"));
        }
        Ok(format!("TCP_ULP reads {:?}", String::from_utf8_lossy(&buf[..l as usize]).trim_end_matches('\0')))
    });
    probe("1.6", "ulp-set-twice(expect-reject)", || {
        need_ulp()?;
        let (a, _b) = pair()?;
        ulp(a.as_raw_fd())?;
        match ulp(a.as_raw_fd()) {
            Ok(()) => Err("UNEXPECTED: second attach succeeded".into()),
            Err(e) => Ok(format!("rejected: {e}")),
        }
    });
    probe("1.7", "tls-opt-without-ulp(expect-reject)", || {
        let (a, _b) = pair()?;
        match install(a.as_raw_fd(), TLS_TX, V13, &keys(Cipher::Chacha, 1), 0) {
            Ok(()) => Err("UNEXPECTED: TLS_TX accepted without ULP".into()),
            Err(e) => Ok(format!("rejected: {e}")),
        }
    });
}

fn p2() {
    let combos = [
        (V13, Cipher::Chacha, "tls13-chacha20poly1305"),
        (V13, Cipher::Aes128, "tls13-aes128gcm"),
        (V13, Cipher::Aes256, "tls13-aes256gcm"),
        (V12, Cipher::Chacha, "tls12-chacha20poly1305"),
    ];
    for (i, (ver, c, name)) in combos.iter().enumerate() {
        let k = keys(*c, 10 + i as u8);
        let mut conn = None;
        probe(&format!("2.{}.0", i + 1), &format!("{name}-install-tx+rx"), || {
            let p = tls_pair(*ver, &k)?;
            conn = Some(p);
            Ok("TLS_TX on A and TLS_RX on B accepted".into())
        });
        for (j, n) in [1usize, 100, 16384, 16385, 1 << 20].iter().enumerate() {
            probe(&format!("2.{}.{}", i + 1, j + 1), &format!("{name}-send-{n}"), || {
                let (a, b) = conn.as_ref().ok_or("SKIP install failed")?;
                xfer(a.as_raw_fd(), b.as_raw_fd(), *n, j as u32 + 1)
            });
        }
    }
}

fn p3() {
    for (i, c) in [Cipher::Chacha, Cipher::Aes128, Cipher::Aes256].iter().enumerate() {
        probe(&format!("3.{}", i + 1), &format!("wire-tx-{}-userspace-decrypt", c.name()), || {
            need_ulp()?;
            let k = keys(*c, 30 + i as u8);
            let (a, b) = pair()?;
            ulp(a.as_raw_fd())?;
            install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
            let m0 = b"hello kTLS record zero";
            write_all(a.as_raw_fd(), m0)?;
            let r0 = read_record(b.as_raw_fd())?;
            let hdr_ok = r0[0] == 23 && r0[1] == 3 && r0[2] == 3;
            let (ty0, pt0, pad0) = open13(&k, 0, &r0).map_err(|e| format!("record 0: {e}"))?;
            let m1 = b"second";
            write_all(a.as_raw_fd(), m1)?;
            let r1 = read_record(b.as_raw_fd())?;
            let (ty1, pt1, _) = open13(&k, 1, &r1).map_err(|e| format!("record 1: {e}"))?;
            if !hdr_ok || ty0 != 23 || pt0 != m0 || ty1 != 23 || pt1 != m1 {
                return Err(format!("mismatch: hdr {:02x?} ty0 {ty0} ty1 {ty1}", &r0[..5]));
            }
            Ok(format!(
                "hdr {:02x?} (type 23, 0x0303, len {} = {}+1+16), inner type 23, pad {pad0}, seq 0 and 1 decrypt with nonce=iv^seq, AAD=header",
                &r0[..5],
                r0.len() - 5,
                m0.len()
            ))
        });
    }
    probe("3.4", "wire-userspace-record-into-rx", || {
        need_ulp()?;
        let k = keys(Cipher::Chacha, 34);
        let (a, b) = pair()?;
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        let m0 = b"from userspace, no padding";
        let m1 = b"from userspace, 10 bytes padding";
        let mut w = seal13(&k, 0, 23, m0, 0);
        w.extend(seal13(&k, 1, 23, m1, 10));
        write_all(a.as_raw_fd(), &w)?;
        let got = read_exact(b.as_raw_fd(), m0.len() + m1.len())?;
        let mut want = m0.to_vec();
        want.extend(m1);
        if got != want {
            return Err(format!("mismatch: {}", first_diff(&got, &want)));
        }
        Ok("two userspace-sealed records (one padded) read through TLS_RX".into())
    });
    probe("3.5", "wire-userspace-aes128-into-rx", || {
        need_ulp()?;
        let k = keys(Cipher::Aes128, 35);
        let (a, b) = pair()?;
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        let m0 = b"aes record";
        write_all(a.as_raw_fd(), &seal13(&k, 0, 23, m0, 0))?;
        let got = read_exact(b.as_raw_fd(), m0.len())?;
        if got != m0 {
            return Err("mismatch".into());
        }
        Ok("userspace AES-128-GCM record read through TLS_RX".into())
    });
}

fn p4() {
    const N: usize = 1 << 20;
    let data = pattern(N, 4);
    probe("4.1", "sendfile-1MiB-to-ktls-rx", || {
        let k = keys(Cipher::Chacha, 41);
        let (a, b) = tls_pair(V13, &k)?;
        let f = tmpfile(&data)?;
        let (afd, ffd) = (a.as_raw_fd(), f.as_raw_fd());
        let h = thread::spawn(move || sendfile_all(afd, ffd, N));
        let got = read_exact(b.as_raw_fd(), N);
        let calls = h.join().unwrap()?;
        let got = got?;
        if got != data {
            return Err(format!("mismatch: {}", first_diff(&got, &data)));
        }
        Ok(format!("1 MiB via {calls} sendfile call(s), identical"))
    });
    probe("4.2", "sendfile-1MiB-record-sizes-on-wire", || {
        need_ulp()?;
        let k = keys(Cipher::Chacha, 42);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        let f = tmpfile(&data)?;
        let (afd, ffd) = (a.as_raw_fd(), f.as_raw_fd());
        let h = thread::spawn(move || sendfile_all(afd, ffd, N));
        let got = read_records_plain(b.as_raw_fd(), &k, N);
        h.join().unwrap()?;
        let (got, lens) = got?;
        if got != data {
            return Err(format!("mismatch: {}", first_diff(&got, &data)));
        }
        Ok(format!("all decrypted in userspace; {}", summarize_lens(&lens)))
    });
    probe("4.3", "write-1MiB-record-sizes-on-wire", || {
        need_ulp()?;
        let k = keys(Cipher::Chacha, 43);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        let d2 = data.clone();
        let afd = a.as_raw_fd();
        let h = thread::spawn(move || write_all(afd, &d2));
        let got = read_records_plain(b.as_raw_fd(), &k, N);
        h.join().unwrap()?;
        let (got, lens) = got?;
        if got != data {
            return Err(format!("mismatch: {}", first_diff(&got, &data)));
        }
        Ok(format!("one send() of 1 MiB; {}", summarize_lens(&lens)))
    });
    probe("4.4", "splice-pipe-to-ktls-tx", || {
        let k = keys(Cipher::Chacha, 44);
        let (a, b) = tls_pair(V13, &k)?;
        let total = 256 * 1024;
        let d = pattern(total, 44);
        let d2 = d.clone();
        let afd = a.as_raw_fd();
        let h = thread::spawn(move || -> Result<(), String> {
            let mut p = [0; 2];
            if unsafe { libc::pipe(p.as_mut_ptr()) } < 0 {
                return Err(syserr("pipe"));
            }
            let mut res = Ok(());
            for chunk in d2.chunks(16384) {
                let w = unsafe { libc::write(p[1], chunk.as_ptr() as *const _, chunk.len()) };
                if w != chunk.len() as isize {
                    res = Err(syserr("write(pipe)"));
                    break;
                }
                let mut left = chunk.len();
                while left > 0 {
                    let r = unsafe { libc::splice(p[0], std::ptr::null_mut(), afd, std::ptr::null_mut(), left, 0) };
                    if r <= 0 {
                        res = Err(syserr("splice(pipe->ktls socket)"));
                        break;
                    }
                    left -= r as usize;
                }
                if res.is_err() {
                    break;
                }
            }
            unsafe {
                libc::close(p[0]);
                libc::close(p[1]);
            }
            res
        });
        let got = read_exact(b.as_raw_fd(), total);
        h.join().unwrap()?;
        let got = got?;
        if got != d {
            return Err(format!("mismatch: {}", first_diff(&got, &d)));
        }
        Ok("256 KiB spliced from a pipe in 16 KiB chunks, identical".into())
    });
    probe("4.5", "tx-zerocopy-ro-setsockopt", || {
        let k = keys(Cipher::Chacha, 45);
        let (a, _b) = tls_pair(V13, &k)?;
        set_int(a.as_raw_fd(), SOL_TLS, TLS_TX_ZEROCOPY_RO, 1, "setsockopt(SOL_TLS,TLS_TX_ZEROCOPY_RO,1)")?;
        let v = get_int(a.as_raw_fd(), SOL_TLS, TLS_TX_ZEROCOPY_RO, "getsockopt(TLS_TX_ZEROCOPY_RO)");
        Ok(format!("accepted; getsockopt -> {v:?}"))
    });
    probe("4.6", "sendfile-with-zerocopy-ro", || {
        let k = keys(Cipher::Chacha, 46);
        let (a, b) = tls_pair(V13, &k)?;
        set_int(a.as_raw_fd(), SOL_TLS, TLS_TX_ZEROCOPY_RO, 1, "setsockopt(SOL_TLS,TLS_TX_ZEROCOPY_RO,1)")?;
        let f = tmpfile(&data)?;
        let (afd, ffd) = (a.as_raw_fd(), f.as_raw_fd());
        let h = thread::spawn(move || sendfile_all(afd, ffd, N));
        let got = read_exact(b.as_raw_fd(), N);
        h.join().unwrap()?;
        let got = got?;
        if got != data {
            return Err(format!("mismatch: {}", first_diff(&got, &data)));
        }
        Ok("1 MiB identical (software kTLS: option has no effect expected)".into())
    });
    probe("4.7", "tx-zerocopy-ro-before-tls-tx", || {
        need_ulp()?;
        let (a, _b) = pair()?;
        ulp(a.as_raw_fd())?;
        match set_int(a.as_raw_fd(), SOL_TLS, TLS_TX_ZEROCOPY_RO, 1, "setsockopt(SOL_TLS,TLS_TX_ZEROCOPY_RO,1)") {
            Ok(()) => Ok("accepted before TLS_TX".into()),
            Err(e) => Ok(format!("rejected before TLS_TX: {e}")),
        }
    });
}

fn p5() {
    let hs: Vec<u8> = vec![8, 0, 0, 2, 0, 0]; // EncryptedExtensions, empty
    probe("5.1", "handshake-record-type22-roundtrip", || {
        let k = keys(Cipher::Chacha, 51);
        let (a, b) = tls_pair(V13, &k)?;
        send_rec(a.as_raw_fd(), 22, &hs)?;
        let (d, ty, fl) = recv_rec(b.as_raw_fd(), 16384)?;
        if ty != Some(22) || d != hs {
            return Err(format!("got type {ty:?} data {d:02x?}"));
        }
        Ok(format!("recvmsg: cmsg TLS_GET_RECORD_TYPE=22, {} bytes, msg_flags={fl:#x}", d.len()))
    });
    probe("5.2", "alert-record-type21-roundtrip", || {
        let k = keys(Cipher::Chacha, 52);
        let (a, b) = tls_pair(V13, &k)?;
        send_rec(a.as_raw_fd(), 21, &[1, 0])?;
        let (d, ty, fl) = recv_rec(b.as_raw_fd(), 16384)?;
        if ty != Some(21) || d != [1, 0] {
            return Err(format!("got type {ty:?} data {d:02x?}"));
        }
        Ok(format!("recvmsg: cmsg type 21, data 01 00, msg_flags={fl:#x}"))
    });
    probe("5.3", "plain-recv-on-control-record", || {
        let k = keys(Cipher::Chacha, 53);
        let (a, b) = tls_pair(V13, &k)?;
        send_rec(a.as_raw_fd(), 22, &hs)?;
        sleep_ms(100);
        let first = match recv1(b.as_raw_fd(), 16384) {
            Ok(d) => format!("plain recv returned {} bytes (!)", d.len()),
            Err(e) => format!("plain recv: {e}"),
        };
        let second = match recv_rec(b.as_raw_fd(), 16384) {
            Ok((d, ty, _)) => format!("then recvmsg+cmsg: type {ty:?}, {} bytes (record not lost)", d.len()),
            Err(e) => format!("then recvmsg+cmsg: {e}"),
        };
        Ok(format!("{first}; {second}"))
    });
    probe("5.4", "socket-after-alert", || {
        let k = keys(Cipher::Chacha, 54);
        let (a, b) = tls_pair(V13, &k)?;
        send_rec(a.as_raw_fd(), 21, &[2, 40])?;
        let tx_after = match write_all(a.as_raw_fd(), b"after-alert") {
            Ok(()) => "A send after alert ok".to_string(),
            Err(e) => format!("A send after alert: {e}"),
        };
        let r1 = recv_rec(b.as_raw_fd(), 16384).map(|(d, t, _)| format!("type {t:?} {:02x?}", d));
        let r2 = recv_rec(b.as_raw_fd(), 16384).map(|(d, t, _)| format!("type {t:?} {:?}", String::from_utf8_lossy(&d)));
        Ok(format!("{tx_after}; B 1st recvmsg: {r1:?}; B 2nd recvmsg: {r2:?}"))
    });
    probe("5.5", "no-mixing-data-control-data", || {
        let k = keys(Cipher::Chacha, 55);
        let (a, b) = tls_pair(V13, &k)?;
        write_all(a.as_raw_fd(), b"data1")?;
        send_rec(a.as_raw_fd(), 22, &hs)?;
        write_all(a.as_raw_fd(), b"data2")?;
        write_all(a.as_raw_fd(), b"data3")?;
        sleep_ms(100);
        let mut seq = Vec::new();
        for _ in 0..3 {
            match recv_rec(b.as_raw_fd(), 16384) {
                Ok((d, t, _)) => seq.push(format!("({t:?},{}B)", d.len())),
                Err(e) => {
                    seq.push(e);
                    break;
                }
            }
        }
        Ok(format!("recvmsg sequence (type,len): {}", seq.join(" ")))
    });
    probe("5.6", "control-record-on-wire", || {
        need_ulp()?;
        let k = keys(Cipher::Chacha, 56);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        send_rec(a.as_raw_fd(), 22, &hs)?;
        send_rec(a.as_raw_fd(), 21, &[1, 0])?;
        let r0 = read_record(b.as_raw_fd())?;
        let (t0, p0, _) = open13(&k, 0, &r0)?;
        let r1 = read_record(b.as_raw_fd())?;
        let (t1, p1, _) = open13(&k, 1, &r1)?;
        if r0[0] != 23 || t0 != 22 || p0 != hs || r1[0] != 23 || t1 != 21 || p1 != [1, 0] {
            return Err(format!("outer {} inner {t0}; outer {} inner {t1}", r0[0], r1[0]));
        }
        Ok("outer type 23, inner type 22 then 21; seq 0,1".into())
    });
    probe("5.7", "receive-userspace-alert-close-notify-then-eof", || {
        need_ulp()?;
        let k = keys(Cipher::Chacha, 57);
        let (a, b) = pair()?;
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        write_all(a.as_raw_fd(), &seal13(&k, 0, 21, &[1, 0], 0))?;
        unsafe { libc::shutdown(a.as_raw_fd(), libc::SHUT_WR) };
        let r1 = recv_rec(b.as_raw_fd(), 100).map(|(d, t, _)| format!("type {t:?} {d:02x?}"));
        let r2 = recv_rec(b.as_raw_fd(), 100).map(|(d, t, _)| format!("type {t:?} {} bytes", d.len()));
        Ok(format!("1st: {r1:?}; 2nd: {r2:?}"))
    });
}

fn p6() {
    let c = Cipher::Chacha;
    probe("6.1", "tx-rekey-setsockopt", || {
        let (k1, k2) = (keys(c, 61), keys(c, 62));
        let (a, b) = tls_pair(V13, &k1)?;
        xfer(a.as_raw_fd(), b.as_raw_fd(), 10, 1)?;
        install(a.as_raw_fd(), TLS_TX, V13, &k2, 0)?;
        Ok("second TLS_TX accepted".into())
    });
    probe("6.2", "rx-rekey-setsockopt-without-keyupdate", || {
        let (k1, k2) = (keys(c, 63), keys(c, 64));
        let (_a, b) = tls_pair(V13, &k1)?;
        install(b.as_raw_fd(), TLS_RX, V13, &k2, 0)?;
        Ok("second TLS_RX accepted".into())
    });
    let ku_flow = |id: &str, name: &str, queued: bool| {
        probe(id, name, || {
            let (k1, k2) = (keys(c, 65), keys(c, 66));
            let (a, b) = tls_pair(V13, &k1)?;
            let (afd, bfd) = (a.as_raw_fd(), b.as_raw_fd());
            let mut log = Vec::new();
            write_all(afd, b"old")?;
            if !queued {
                let g = read_exact(bfd, 3)?;
                log.push(format!("B read old={:?}", String::from_utf8_lossy(&g)));
            }
            send_rec(afd, 22, &KEY_UPDATE)?;
            install(afd, TLS_TX, V13, &k2, 0)?;
            write_all(afd, b"new")?;
            sleep_ms(100);
            if queued {
                let g = recv_rec(bfd, 3).map_err(|e| format!("read old: {e}"))?;
                log.push(format!("B read old: type {:?} {:?}", g.1, String::from_utf8_lossy(&g.0)));
            }
            let ku = recv_rec(bfd, 100).map_err(|e| format!("recvmsg KeyUpdate: {e}"))?;
            log.push(format!("B recvmsg: type {:?} {:02x?}", ku.1, ku.0));
            let mut pfd = libc::pollfd { fd: bfd, events: libc::POLLIN, revents: 0 };
            let pr = unsafe { libc::poll(&mut pfd, 1, 50) };
            log.push(format!("poll before new key -> {pr} revents={:#x}", pfd.revents));
            match recv1(bfd, 100) {
                Ok(d) => log.push(format!("recv before new RX key returned {} bytes {:?}", d.len(), String::from_utf8_lossy(&d))),
                Err(e) => log.push(format!("recv before new RX key: {e}")),
            }
            install(bfd, TLS_RX, V13, &k2, 0).map_err(|e| format!("{}; RX rekey: {e}", log.join("; ")))?;
            log.push("RX rekey ok".into());
            let n = read_exact(bfd, 3).map_err(|e| format!("{}; read new: {e}", log.join("; ")))?;
            if n != b"new" {
                return Err(format!("{}; new data mismatch {n:?}", log.join("; ")));
            }
            log.push("B read new ok".into());
            Ok(log.join("; "))
        });
    };
    ku_flow("6.3", "rekey-with-keyupdate-stepwise", false);
    ku_flow("6.4", "rekey-with-keyupdate-all-queued", true);
    probe("6.5", "rekey-no-keyupdate-old-read-first", || {
        let (k1, k2) = (keys(c, 67), keys(c, 68));
        let (a, b) = tls_pair(V13, &k1)?;
        let (afd, bfd) = (a.as_raw_fd(), b.as_raw_fd());
        write_all(afd, b"old")?;
        read_exact(bfd, 3)?;
        install(afd, TLS_TX, V13, &k2, 0)?;
        write_all(afd, b"new")?;
        sleep_ms(100);
        install(bfd, TLS_RX, V13, &k2, 0)?;
        let n = read_exact(bfd, 3)?;
        if n != b"new" {
            return Err("new data mismatch".into());
        }
        Ok("RX rekey without KeyUpdate accepted; new-key record (already queued) read ok".into())
    });
    probe("6.6", "rekey-no-keyupdate-old-unread(expect-break)", || {
        let (k1, k2) = (keys(c, 69), keys(c, 70));
        let (a, b) = tls_pair(V13, &k1)?;
        let (afd, bfd) = (a.as_raw_fd(), b.as_raw_fd());
        write_all(afd, b"old")?;
        install(afd, TLS_TX, V13, &k2, 0)?;
        write_all(afd, b"new")?;
        sleep_ms(100);
        install(bfd, TLS_RX, V13, &k2, 0)?;
        let r = recv1(bfd, 100);
        let r2 = recv1(bfd, 100);
        Ok(format!("1st recv: {:?}; 2nd recv: {:?}; {}", r.map(|d| String::from_utf8_lossy(&d).to_string()), r2.map(|d| String::from_utf8_lossy(&d).to_string()), so_error(bfd)))
    });
    probe("6.7", "handshake-to-app-key-switch-all-queued", || {
        let (hk, ak) = (keys(c, 71), keys(c, 72));
        let (a, b) = tls_pair(V13, &hk)?;
        let (afd, bfd) = (a.as_raw_fd(), b.as_raw_fd());
        let mut fin = vec![20u8, 0, 0, 32];
        fin.extend([0x5a; 32]);
        send_rec(afd, 22, &fin)?;
        install(afd, TLS_TX, V13, &ak, 0)?;
        write_all(afd, b"first application data")?;
        sleep_ms(100);
        let (d, t, _) = recv_rec(bfd, 16384)?;
        if t != Some(22) || d != fin {
            return Err(format!("expected Finished, got type {t:?} {} bytes", d.len()));
        }
        install(bfd, TLS_RX, V13, &ak, 0)?;
        let g = read_exact(bfd, 22)?;
        if g != b"first application data" {
            return Err("app data mismatch".into());
        }
        Ok("Finished (hs key) read via recvmsg, RX switched to app key, queued app record read ok".into())
    });
    probe("6.8", "rekey-change-cipher(expect-reject)", || {
        let (a, _b) = tls_pair(V13, &keys(c, 73))?;
        match install(a.as_raw_fd(), TLS_TX, V13, &keys(Cipher::Aes128, 74), 0) {
            Ok(()) => Err("UNEXPECTED: cipher change accepted".into()),
            Err(e) => Ok(format!("rejected: {e}")),
        }
    });
    probe("6.9", "tls12-rekey(expect-reject)", || {
        let (a, _b) = tls_pair(V12, &keys(c, 75))?;
        match install(a.as_raw_fd(), TLS_TX, V12, &keys(c, 76), 0) {
            Ok(()) => Err("UNEXPECTED: TLS 1.2 rekey accepted".into()),
            Err(e) => Ok(format!("rejected: {e}")),
        }
    });
    probe("6.10", "rekey-wire-seq-resets", || {
        need_ulp()?;
        let (k1, k2) = (keys(c, 77), keys(c, 78));
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k1, 0)?;
        write_all(a.as_raw_fd(), b"x")?;
        write_all(a.as_raw_fd(), b"y")?;
        let r0 = read_record(b.as_raw_fd())?;
        open13(&k1, 0, &r0)?;
        let r1 = read_record(b.as_raw_fd())?;
        open13(&k1, 1, &r1)?;
        install(a.as_raw_fd(), TLS_TX, V13, &k2, 0)?;
        write_all(a.as_raw_fd(), b"z")?;
        let r2 = read_record(b.as_raw_fd())?;
        let (_, p, _) = open13(&k2, 0, &r2).map_err(|e| format!("new-key record with seq 0: {e}"))?;
        Ok(format!("after rekey the next record uses new key with the given rec_seq 0 ({:?})", String::from_utf8_lossy(&p)))
    });
    probe("6.11", "tx-rekey-with-unsent-msg_more-data", || {
        let (k1, k2) = (keys(c, 79), keys(c, 80));
        let (a, b) = tls_pair(V13, &k1)?;
        let (afd, bfd) = (a.as_raw_fd(), b.as_raw_fd());
        let d = b"pending";
        let r = unsafe { libc::send(afd, d.as_ptr() as *const _, d.len(), libc::MSG_MORE) };
        if r < 0 {
            return Err(syserr("send(MSG_MORE)"));
        }
        let res = install(afd, TLS_TX, V13, &k2, 0);
        let got = recv1(bfd, 100);
        Ok(format!("rekey with open MSG_MORE record: {res:?}; B recv: {:?}", got.map(|g| String::from_utf8_lossy(&g).to_string())))
    });
}

fn p7() {
    let c = Cipher::Chacha;
    probe("7.1", "rx-installed-after-record-queued", || {
        need_ulp()?;
        let k = keys(c, 81);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        write_all(a.as_raw_fd(), b"queued-record")?;
        sleep_ms(100);
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        let g = read_exact(b.as_raw_fd(), 13)?;
        if g != b"queued-record" {
            return Err("mismatch".into());
        }
        Ok("record queued (unread) before ULP+TLS_RX was decrypted ok".into())
    });
    probe("7.2", "rx-after-partial-userspace-read(expect-break)", || {
        need_ulp()?;
        let k = keys(c, 82);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        let m = pattern(100, 7);
        write_all(a.as_raw_fd(), &m)?;
        sleep_ms(100);
        read_exact(b.as_raw_fd(), 58)?; // header + about half
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        write_all(a.as_raw_fd(), b"follow-up")?;
        sleep_ms(50);
        let r = recv1(b.as_raw_fd(), 1000);
        let desc = match &r {
            Ok(d) => format!("recv returned {} bytes", d.len()),
            Err(e) => e.clone(),
        };
        let r2 = recv1(b.as_raw_fd(), 1000).map(|d| d.len());
        Ok(format!("after reading 58 of 122 record bytes then TLS_RX: 1st {desc}; 2nd {r2:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("7.3", "rx-installed-after-3-records-queued", || {
        need_ulp()?;
        let k = keys(c, 83);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        let ms = [pattern(10, 1), pattern(2000, 2), pattern(16384, 3)];
        for m in &ms {
            write_all(a.as_raw_fd(), m)?;
        }
        sleep_ms(100);
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        let want: Vec<u8> = ms.concat();
        let g = read_exact(b.as_raw_fd(), want.len())?;
        if g != want {
            return Err(format!("mismatch: {}", first_diff(&g, &want)));
        }
        Ok("3 queued records (10, 2000, 16384 B) decrypted ok".into())
    });
    probe("7.4", "userspace-consumes-exact-record-then-rx-seq1", || {
        need_ulp()?;
        let k = keys(c, 84);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        write_all(a.as_raw_fd(), b"record-0-userspace")?;
        write_all(a.as_raw_fd(), b"record-1-kernel")?;
        sleep_ms(100);
        let r0 = read_record(b.as_raw_fd())?;
        let (_, p0, _) = open13(&k, 0, &r0)?;
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 1)?;
        let g = read_exact(b.as_raw_fd(), 15)?;
        if p0 != b"record-0-userspace" || g != b"record-1-kernel" {
            return Err("mismatch".into());
        }
        Ok("userspace read record 0 exactly; TLS_RX with rec_seq=1 read queued record 1".into())
    });
    probe("7.5", "rx-installed-with-half-record-in-queue", || {
        need_ulp()?;
        let k = keys(c, 85);
        let (a, b) = pair()?;
        let rec = seal13(&k, 0, 23, b"split record", 0);
        write_all(a.as_raw_fd(), &rec[..10])?;
        sleep_ms(50);
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        sleep_ms(50);
        write_all(a.as_raw_fd(), &rec[10..])?;
        let g = read_exact(b.as_raw_fd(), 12)?;
        if g != b"split record" {
            return Err("mismatch".into());
        }
        Ok("first 10 record bytes queued before TLS_RX, rest after: ok".into())
    });
}

fn p8() {
    let c = Cipher::Chacha;
    probe("8.1", "keys-from-other-process-over-socketpair", || {
        need_ulp()?;
        let k = keys(c, 91);
        let (s0, s1) = socketpair()?;
        let info = cinfo(V13, &k, 0);
        let info2 = info.clone();
        let pid = fork_child(move || {
            unsafe { libc::close(s0) };
            if write_all(s1, &info2).is_err() { 1 } else { 0 }
        })?;
        unsafe { libc::close(s1) };
        let got = read_exact(s0, info.len());
        unsafe { libc::close(s0) };
        let st = reap(pid);
        let got = got?;
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        set_raw(a.as_raw_fd(), TLS_TX, &got)?;
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        let x = xfer(a.as_raw_fd(), b.as_raw_fd(), 1000, 3)?;
        Ok(format!("{st}; {x}"))
    });
    let mut shared: Option<(TcpStream, TcpStream)> = None;
    let (k1, k2) = (keys(c, 92), keys(c, 93));
    probe("8.2", "other-process-installs-keys-via-scm_rights", || {
        need_ulp()?;
        let (s0, s1) = socketpair()?;
        let (ck1, ck2) = (k1.clone(), k2.clone());
        let pid = fork_child(move || {
            unsafe { libc::close(s0) };
            let fd = match recv_fd(s1) {
                Ok(fd) => fd,
                Err(_) => return 2,
            };
            let r = ulp(fd)
                .and_then(|_| install(fd, TLS_TX, V13, &ck1, 0))
                .and_then(|_| install(fd, TLS_RX, V13, &ck2, 0));
            let status = [if r.is_ok() { 0u8 } else { errno() as u8 }];
            let _ = write_all(s1, &status);
            unsafe { libc::close(fd) };
            0
        })?;
        unsafe { libc::close(s1) };
        let (a, b) = pair()?;
        send_fd(s0, a.as_raw_fd())?;
        let st = read_exact(s0, 1);
        unsafe { libc::close(s0) };
        let ch = reap(pid);
        let st = st?;
        if st[0] != 0 {
            return Err(format!("child install failed errno {}({}); {ch}", ename(st[0] as i32), st[0]));
        }
        let (afd, bfd) = (a.as_raw_fd(), b.as_raw_fd());
        ulp(bfd)?;
        install(bfd, TLS_RX, V13, &k1, 0)?;
        install(bfd, TLS_TX, V13, &k2, 0)?;
        let x1 = xfer(afd, bfd, 50_000, 5).map_err(|e| format!("P1 send(): {e}"))?;
        let data = pattern(1 << 20, 6);
        let f = tmpfile(&data)?;
        let ffd = f.as_raw_fd();
        let h = thread::spawn(move || sendfile_all(afd, ffd, 1 << 20));
        let g = read_exact(bfd, 1 << 20);
        h.join().unwrap().map_err(|e| format!("P1 sendfile: {e}"))?;
        if g? != data {
            return Err("sendfile data mismatch".into());
        }
        let x2 = xfer(bfd, afd, 20_000, 7).map_err(|e| format!("P1 recv(): {e}"))?;
        shared = Some((a, b));
        Ok(format!("{ch}; P1 send: {x1}; P1 sendfile 1 MiB ok; P1 recv: {x2}"))
    });
    probe("8.3a", "getsockopt-TLS_TX-readback-on-socket-keyed-by-other-process", || {
        let (a, _) = shared.as_ref().ok_or("SKIP 8.2 failed")?;
        // A sent 2 xfers-worth of records; just report fields.
        readback(a.as_raw_fd(), TLS_TX, V13, &k1, 0).map(|s| format!("{s} (rec_seq nonzero expected: data was sent)"))
    });
    probe("8.3b", "getsockopt-TLS_RX-readback-on-socket-keyed-by-other-process", || {
        let (a, _) = shared.as_ref().ok_or("SKIP 8.2 failed")?;
        readback(a.as_raw_fd(), TLS_RX, V13, &k2, 0)
    });
    for (i, cc) in [Cipher::Chacha, Cipher::Aes128, Cipher::Aes256].iter().enumerate() {
        probe(&format!("8.3{}", ["c", "d", "e"][i]), &format!("getsockopt-readback-fresh-{}", cc.name()), || {
            let k = keys(*cc, 94 + i as u8);
            let (a, b) = tls_pair(V13, &k)?;
            let t = readback(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
            let r = readback(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
            Ok(format!("TX: {t} | RX: {r}"))
        });
    }
    probe("8.3f", "getsockopt-readback-header-only", || {
        let k = keys(c, 97);
        let (a, _b) = tls_pair(V13, &k)?;
        let mut buf = [0u8; 4];
        let mut l: libc::socklen_t = 4;
        let r = unsafe { libc::getsockopt(a.as_raw_fd(), SOL_TLS, TLS_TX, buf.as_mut_ptr() as *mut _, &mut l) };
        if r < 0 {
            return Err(syserr("getsockopt(TLS_TX,len=4)"));
        }
        Ok(format!("len 4 -> version {:#06x} cipher {}", u16::from_ne_bytes([buf[0], buf[1]]), u16::from_ne_bytes([buf[2], buf[3]])))
    });
    drop(shared);
}

fn p9() {
    let c = Cipher::Chacha;
    let rx_pair = |seed: u8| -> Result<(Keys, TcpStream, TcpStream), String> {
        need_ulp()?;
        let k = keys(c, seed);
        let (a, b) = pair()?;
        ulp(b.as_raw_fd())?;
        install(b.as_raw_fd(), TLS_RX, V13, &k, 0)?;
        Ok((k, a, b))
    };
    probe("9.1", "corrupted-tag", || {
        let (k, a, b) = rx_pair(101)?;
        let mut rec = seal13(&k, 0, 23, b"tampered", 0);
        *rec.last_mut().unwrap() ^= 1;
        write_all(a.as_raw_fd(), &rec)?;
        let r1 = recv1(b.as_raw_fd(), 100);
        let after1 = so_error(b.as_raw_fd());
        let _ = write_all(a.as_raw_fd(), &seal13(&k, 1, 23, b"valid next", 0));
        sleep_ms(50);
        let r2 = recv1(b.as_raw_fd(), 100).map(|d| d.len());
        let w = write_all(b.as_raw_fd(), b"x").map(|_| "ok");
        Ok(format!("1st recv: {:?}; then {after1}; 2nd recv (valid seq1 record): {r2:?}; B send: {w:?}", r1.map(|d| d.len())))
    });
    probe("9.2", "oversize-record-header(16641)", || {
        let (_k, a, b) = rx_pair(102)?;
        let len = 16384 + 256 + 1;
        let mut w = vec![23u8, 3, 3, (len >> 8) as u8, len as u8];
        w.extend(vec![0u8; len]);
        let afd = a.as_raw_fd();
        let h = thread::spawn(move || write_all(afd, &w));
        let r = recv1(b.as_raw_fd(), 20000).map(|d| d.len());
        let _ = h.join();
        Ok(format!("recv: {r:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("9.3", "valid-tag-plaintext-16385(overflow)", || {
        let (k, a, b) = rx_pair(103)?;
        let rec = seal13(&k, 0, 23, &pattern(16385, 1), 0);
        let afd = a.as_raw_fd();
        let h = thread::spawn(move || write_all(afd, &rec));
        let r = recv1(b.as_raw_fd(), 20000).map(|d| d.len());
        let _ = h.join();
        Ok(format!("recv: {r:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("9.4", "valid-tag-16384+239-padding(inner>2^14+1)", || {
        let (k, a, b) = rx_pair(104)?;
        let rec = seal13(&k, 0, 23, &pattern(16384, 1), 239);
        let afd = a.as_raw_fd();
        let h = thread::spawn(move || write_all(afd, &rec));
        let r = recv1(b.as_raw_fd(), 20000).map(|d| d.len());
        let _ = h.join();
        Ok(format!("record len {} ; recv: {r:?}; {}", 16384 + 1 + 239 + 16, so_error(b.as_raw_fd())))
    });
    probe("9.5", "truncated-record-then-fin", || {
        let (k, a, b) = rx_pair(105)?;
        let rec = seal13(&k, 0, 23, &pattern(100, 1), 0);
        write_all(a.as_raw_fd(), &rec[..60])?;
        unsafe { libc::shutdown(a.as_raw_fd(), libc::SHUT_WR) };
        let r = recv1(b.as_raw_fd(), 1000).map(|d| d.len());
        let r2 = recv1(b.as_raw_fd(), 1000).map(|d| d.len());
        Ok(format!("1st recv: {r:?}; 2nd: {r2:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("9.6", "rx-expect-no-pad", || {
        let (k, a, b) = rx_pair(106)?;
        let s = set_int(b.as_raw_fd(), SOL_TLS, TLS_RX_EXPECT_NO_PAD, 1, "setsockopt(SOL_TLS,TLS_RX_EXPECT_NO_PAD,1)");
        let g = get_int(b.as_raw_fd(), SOL_TLS, TLS_RX_EXPECT_NO_PAD, "getsockopt(TLS_RX_EXPECT_NO_PAD)");
        let mut w = seal13(&k, 0, 23, b"unpadded", 0);
        w.extend(seal13(&k, 1, 23, b"padded!!", 32));
        write_all(a.as_raw_fd(), &w)?;
        let r = read_exact(b.as_raw_fd(), 16);
        let ok = r.as_deref().map(|d| d == b"unpaddedpadded!!").unwrap_or(false);
        Ok(format!("setsockopt: {s:?}; getsockopt: {g:?}; unpadded+padded records read: {}", if ok { "ok".to_string() } else { format!("{r:?}") }))
    });
    probe("9.7", "zero-length-app-record-at-rx", || {
        let (k, a, b) = rx_pair(107)?;
        let mut w = seal13(&k, 0, 23, b"", 0);
        w.extend(seal13(&k, 1, 23, b"after", 0));
        write_all(a.as_raw_fd(), &w)?;
        sleep_ms(50);
        let r1 = recv1(b.as_raw_fd(), 100).map(|d| format!("{} bytes {:?}", d.len(), String::from_utf8_lossy(&d)));
        let r2 = recv1(b.as_raw_fd(), 100).map(|d| format!("{} bytes", d.len()));
        Ok(format!("1st recv: {r1:?}; 2nd recv: {r2:?}"))
    });
    probe("9.8", "zero-length-send-at-tx", || {
        need_ulp()?;
        let k = keys(c, 108);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        let r = unsafe { libc::send(a.as_raw_fd(), b"".as_ptr() as *const _, 0, 0) };
        let sr = if r < 0 { syserr("send(len 0)") } else { format!("send(len 0) -> {r}") };
        write_all(a.as_raw_fd(), b"z")?;
        let rec = read_record(b.as_raw_fd())?;
        let (t, p, _) = open13(&k, 0, &rec)?;
        Ok(format!("{sr}; first record on wire: type {t}, {} plaintext bytes ({})", p.len(), if p.is_empty() { "EMPTY RECORD EMITTED" } else { "no empty record" }))
    });
    probe("9.9", "wrong-header-version-0x0301", || {
        let (k, a, b) = rx_pair(109)?;
        let mut rec = seal13(&k, 0, 23, b"v", 0);
        rec[2] = 1;
        write_all(a.as_raw_fd(), &rec)?;
        let r = recv1(b.as_raw_fd(), 100).map(|d| d.len());
        Ok(format!("recv: {r:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("9.10", "plaintext-ccs-record-at-rx", || {
        let (k, a, b) = rx_pair(110)?;
        let mut w = vec![20u8, 3, 3, 0, 1, 1];
        w.extend(seal13(&k, 0, 23, b"after ccs", 0));
        write_all(a.as_raw_fd(), &w)?;
        let r = recv_rec(b.as_raw_fd(), 100).map(|(d, t, _)| format!("type {t:?} {} bytes", d.len()));
        Ok(format!("recvmsg: {r:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("9.11", "wrong-sequence-number", || {
        let (k, a, b) = rx_pair(111)?;
        write_all(a.as_raw_fd(), &seal13(&k, 5, 23, b"seq5", 0))?;
        let r = recv1(b.as_raw_fd(), 100).map(|d| d.len());
        Ok(format!("recv: {r:?}; {}", so_error(b.as_raw_fd())))
    });
    probe("9.12", "small-recv-buffer-partial-record", || {
        let (k, a, b) = rx_pair(112)?;
        write_all(a.as_raw_fd(), &seal13(&k, 0, 23, &pattern(1000, 9), 0))?;
        let r1 = recv1(b.as_raw_fd(), 10)?;
        let r2 = read_exact(b.as_raw_fd(), 990)?;
        let mut all = r1.clone();
        all.extend(r2);
        if all != pattern(1000, 9) {
            return Err("mismatch".into());
        }
        Ok(format!("1000-byte record read as {} + 990 bytes", r1.len()))
    });
}

fn p10() {
    let c = Cipher::Chacha;
    probe("10.1", "shutdown-wr-on-tx-peer-plain", || {
        need_ulp()?;
        let k = keys(c, 121);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        write_all(a.as_raw_fd(), b"last")?;
        let s = unsafe { libc::shutdown(a.as_raw_fd(), libc::SHUT_WR) };
        let s = if s < 0 { syserr("shutdown") } else { "shutdown ok".into() };
        let rec = read_record(b.as_raw_fd())?;
        open13(&k, 0, &rec)?;
        let mut extra = Vec::new();
        loop {
            match recv1(b.as_raw_fd(), 4096) {
                Ok(d) if d.is_empty() => break,
                Ok(d) => extra.extend(d),
                Err(e) => return Err(format!("{s}; after data record: {e}")),
            }
        }
        let w = write_all(a.as_raw_fd(), b"more").map(|_| "ok");
        Ok(format!("{s}; peer saw data record then EOF with {} extra bytes{}; send after shutdown: {w:?}", extra.len(),
            if extra.len() >= 5 { format!(" (first record type {} len {})", extra[0], u16::from_be_bytes([extra[3], extra[4]])) } else { String::new() }))
    });
    probe("10.2", "close-on-tx-peer-plain", || {
        need_ulp()?;
        let k = keys(c, 122);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        write_all(a.as_raw_fd(), b"last")?;
        drop(a);
        let rec = read_record(b.as_raw_fd())?;
        open13(&k, 0, &rec)?;
        let mut extra = 0;
        loop {
            match recv1(b.as_raw_fd(), 4096) {
                Ok(d) if d.is_empty() => break,
                Ok(d) => extra += d.len(),
                Err(e) => return Err(format!("after data record: {e}")),
            }
        }
        Ok(format!("peer saw data record then EOF with {extra} extra bytes"))
    });
    probe("10.3", "shutdown-wr-peer-ktls-rx-sees", || {
        let k = keys(c, 123);
        let (a, b) = tls_pair(V13, &k)?;
        write_all(a.as_raw_fd(), b"last")?;
        unsafe { libc::shutdown(a.as_raw_fd(), libc::SHUT_WR) };
        let r1 = recv_rec(b.as_raw_fd(), 100).map(|(d, t, _)| format!("{} bytes type {t:?}", d.len()));
        let r2 = recv_rec(b.as_raw_fd(), 100).map(|(d, t, _)| format!("{} bytes type {t:?}", d.len()));
        Ok(format!("1st recvmsg: {r1:?}; 2nd recvmsg: {r2:?}"))
    });
    probe("10.4", "userspace-close-notify-via-cmsg-then-shutdown", || {
        need_ulp()?;
        let k = keys(c, 124);
        let (a, b) = pair()?;
        ulp(a.as_raw_fd())?;
        install(a.as_raw_fd(), TLS_TX, V13, &k, 0)?;
        send_rec(a.as_raw_fd(), 21, &[1, 0])?;
        unsafe { libc::shutdown(a.as_raw_fd(), libc::SHUT_WR) };
        let rec = read_record(b.as_raw_fd())?;
        let (t, p, _) = open13(&k, 0, &rec)?;
        let eof = recv1(b.as_raw_fd(), 100).map(|d| d.len());
        Ok(format!("close_notify sent by app: inner type {t} {p:02x?}; then recv {eof:?} (0 = EOF)"))
    });
}

fn main() {
    let label = std::env::args().nth(1).unwrap_or_else(|| "unlabelled".into());
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut u) };
    let rel = unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }.to_string_lossy().to_string();
    let mach = unsafe { std::ffi::CStr::from_ptr(u.machine.as_ptr()) }.to_string_lossy().to_string();
    println!(
        "SPIKE label={label} uid={} euid={} kernel={rel} machine={mach}",
        unsafe { libc::getuid() },
        unsafe { libc::geteuid() }
    );
    let stat0 = read_tls_stat();
    p1();
    p2();
    p3();
    p4();
    p5();
    p6();
    p7();
    p8();
    p9();
    p10();
    probe("12", "tls_stat-delta", || {
        let stat1 = read_tls_stat();
        if stat1.is_empty() {
            return Err("SKIP /proc/net/tls_stat not readable or empty".into());
        }
        let d: Vec<String> = stat1
            .iter()
            .filter_map(|(n, v)| {
                let old = stat0.iter().find(|(m, _)| m == n).map(|x| x.1).unwrap_or(0);
                (old != *v).then(|| format!("{n}:{old}->{v}"))
            })
            .collect();
        Ok(d.join(" "))
    });
    println!("SUMMARY label={label} kernel={rel}");
    let res = RESULTS.lock().unwrap();
    let (mut ok, mut fail, mut skip) = (0, 0, 0);
    for (id, name, st) in res.iter() {
        println!("SUMMARY {:<6} {:<4} {name}", id, st);
        match *st {
            "OK" => ok += 1,
            "FAIL" => fail += 1,
            _ => skip += 1,
        }
    }
    println!("SUMMARY totals OK={ok} FAIL={fail} SKIP={skip}");
}
