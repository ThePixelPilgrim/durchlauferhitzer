// THROWAWAY spike: the "server" side.
//
//   server serve --mode spawn|pool|inproc|plain --port P [--helper PATH] [--chain CHAIN.bin]
//          [--pool N] [--spawner root|unpriv] [--key KEY.sec1.der] [--inproc-keyd]
//          [--big FILE] [--no-uidswitch]
//   server hspawn --helper PATH --chain CHAIN.bin [--pool N]   (run as the helper uid)
//   server test-ctl-reject | test-helper-reject --helper PATH --chain CHAIN.bin | probe-keyd
//
// The process starts as root; "serve" forks the spawner (root) first, then
// drops itself to SERVER_UID before accepting connections.

#[path = "../../common/sys.rs"]
mod sys;
mod record;
mod spawner;
mod inproc;

use std::collections::HashMap;
use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use record::{Conn, Dir, Rec};

pub struct Opts {
    pub mode: String,
    pub port: u16,
    pub helper: String,
    pub chain: String,
    pub pool: usize,
    pub spawner: String,
    pub key: String,
    pub inproc_keyd: bool,
    pub big: String,
    pub uidswitch: bool,
    pub verbose: bool,
}

fn opts(args: &[String]) -> Opts {
    let mut m: HashMap<&str, String> = HashMap::new();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--inproc-keyd" | "--no-uidswitch" | "--verbose" => flags.push(a),
            _ => {
                m.insert(a, args.get(i + 1).cloned().unwrap_or_default());
                i += 1;
            }
        }
        i += 1;
    }
    let g = |k: &str, d: &str| m.get(k).cloned().unwrap_or_else(|| d.to_string());
    Opts {
        mode: g("--mode", "spawn"),
        port: g("--port", "8443").parse().unwrap_or(8443),
        helper: g("--helper", ""),
        chain: g("--chain", ""),
        pool: g("--pool", "0").parse().unwrap_or(0),
        spawner: g("--spawner", "root"),
        key: g("--key", ""),
        inproc_keyd: flags.contains(&"--inproc-keyd"),
        big: g("--big", ""),
        uidswitch: !flags.contains(&"--no-uidswitch"),
        verbose: flags.contains(&"--verbose"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).cloned().unwrap_or_default();
    let o = opts(&args[2.min(args.len())..]);
    match cmd.as_str() {
        "serve" => serve(o),
        "hspawn" => spawner::run_unpriv(&o),
        "test-ctl-reject" => test_ctl_reject(),
        "test-helper-reject" => spawner::test_helper_reject(&o),
        "probe-keyd" => probe_keyd(),
        _ => eprintln!("usage: see source"),
    }
}

fn tcp_listen(port: u16) -> RawFd {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        let one: libc::c_int = 1;
        libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, (&one as *const libc::c_int).cast(), 4);
        let mut a: libc::sockaddr_in = std::mem::zeroed();
        a.sin_family = libc::AF_INET as u16;
        a.sin_port = port.to_be();
        a.sin_addr.s_addr = u32::from_be_bytes([127, 0, 0, 1]).to_be();
        if libc::bind(fd, (&a as *const libc::sockaddr_in).cast(), 16) != 0 {
            panic!("bind: {}", io::Error::last_os_error());
        }
        libc::listen(fd, 4096);
        fd
    }
}

pub static STATS_OK: AtomicU64 = AtomicU64::new(0);
pub static STATS_FAIL: AtomicU64 = AtomicU64::new(0);

fn serve(o: Opts) {
    let o = Arc::new(o);
    // 1. the spawner, before containment (root), or connect to an unprivileged one later
    let mut spawner_sock: RawFd = -1;
    if (o.mode == "spawn" || o.mode == "pool") && o.spawner == "root" {
        let (a, b) = sys::seqpacket_pair().expect("pair");
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe { libc::close(a) };
            spawner::run_root(&o, b);
            std::process::exit(0);
        }
        unsafe { libc::close(b) };
        spawner_sock = a;
        eprintln!("server: root spawner pid {pid}");
    }
    let lfd = tcp_listen(o.port);
    if o.mode == "inproc" {
        inproc::prepare(&o);
    }
    // 2. containment of the server itself: drop to SERVER_UID
    sys::drop_to(sys::SERVER_UID).expect("drop");
    if (o.mode == "spawn" || o.mode == "pool") && o.spawner == "unpriv" {
        spawner_sock = sys::connect_abstract(sys::HSPAWN_ABSTRACT).expect("connect hspawn");
        let pc = sys::peercred(spawner_sock).expect("peercred");
        if pc.uid != sys::HELPER_UID {
            panic!("helper spawner has uid {}, expected {}", pc.uid, sys::HELPER_UID);
        }
        eprintln!("server: connected to unprivileged helper spawner pid {} uid {}", pc.pid, pc.uid);
    }
    eprintln!("server: uid {} mode {} listening on 127.0.0.1:{}", unsafe { libc::getuid() }, o.mode, o.port);
    loop {
        let c = unsafe { libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 {
            continue;
        }
        let one: libc::c_int = 1;
        unsafe { libc::setsockopt(c, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&one as *const libc::c_int).cast(), 4) };
        let o = o.clone();
        let sp = spawner_sock;
        std::thread::spawn(move || {
            let r = match o.mode.as_str() {
                "plain" => plain_conn(c, &o),
                "inproc" => inproc::conn(c, &o).and_then(|(conn, v)| serve_tls(conn, &v, &o)),
                _ => via_helper(c, sp).and_then(|(conn, v)| serve_tls(conn, &v, &o)),
            };
            match r {
                Ok(()) => {
                    STATS_OK.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    STATS_FAIL.fetch_add(1, Ordering::Relaxed);
                    eprintln!("server: connection failed: {e}");
                }
            }
        });
    }
}

/// Hand the client to a helper through the spawner; get it back with the secrets.
fn via_helper(client: RawFd, spawner: RawFd) -> io::Result<(Conn, String)> {
    let (a, b) = sys::seqpacket_pair()?;
    sys::set_passcred(a)?;
    let r = sys::send_fds(spawner, b"go", &[client, b]);
    unsafe {
        libc::close(client);
        libc::close(b);
    }
    r?;
    let mut m = [0u8; sys::HANDOVER_LEN + 16];
    let rx = sys::recv_fds(a, &mut m);
    unsafe { libc::close(a) };
    let rx = rx?;
    if rx.len == 0 {
        for f in rx.fds { unsafe { libc::close(f) }; }
        return Err(io::Error::other("helper closed the control channel without a result"));
    }
    let cred = rx.cred.ok_or_else(|| io::Error::other("no SCM_CREDENTIALS"))?;
    if cred.uid != sys::HELPER_UID || rx.len != sys::HANDOVER_LEN || rx.fds.len() != 1 {
        for f in rx.fds { unsafe { libc::close(f) }; }
        return Err(io::Error::other(format!("REJECT handover from uid {} pid {} ({} bytes)", cred.uid, cred.pid, rx.len)));
    }
    let fd = rx.fds[0];
    let v = handover_to_conn(fd, &m[..rx.len])?;
    Ok(v)
}

pub fn handover_to_conn(fd: RawFd, m: &[u8]) -> io::Result<(Conn, String)> {
    let ver = String::from_utf8_lossy(&m[..16]).trim_end_matches('\0').to_string();
    let suite = u16::from_le_bytes([m[16], m[17]]);
    let txseq = u64::from_le_bytes(m[18..26].try_into().unwrap_or_default());
    let rxseq = u64::from_le_bytes(m[26..34].try_into().unwrap_or_default());
    let cs: [u8; 32] = m[34..66].try_into().unwrap_or_default();
    let ss: [u8; 32] = m[66..98].try_into().unwrap_or_default();
    let tx = Dir::from_secret(suite, ss, txseq)?;
    let rx = Dir::from_secret(suite, cs, rxseq)?;
    // Cross-check: keys derived from the KeyLog secrets == rustls's extracted key/iv.
    let kl = if suite == 0x1303 { 32 } else { 16 };
    let (tk, tiv) = tx.key_iv();
    let (rk, riv) = rx.key_iv();
    if tk[..kl] != m[98..98 + kl] || tiv[..] != m[130..142] || rk[..kl] != m[142..142 + kl] || riv[..] != m[174..186] {
        return Err(io::Error::other("derived keys differ from extracted keys"));
    }
    Ok((Conn::new(fd, rx, tx), ver))
}

fn serve_tls(mut c: Conn, ver: &str, o: &Opts) -> io::Result<()> {
    let r = serve_tls_inner(&mut c, ver, o);
    unsafe { libc::close(c.fd) };
    r
}

fn serve_tls_inner(c: &mut Conn, ver: &str, o: &Opts) -> io::Result<()> {
    let mut req = Vec::new();
    loop {
        // read one request head
        let head_end = loop {
            if let Some(p) = find(&req, b"\r\n\r\n") {
                break p + 4;
            }
            match c.recv()? {
                Rec::Data(d) => req.extend_from_slice(&d),
                Rec::Closed => {
                    let _ = c.close_notify();
                    return Ok(());
                }
            }
        };
        let head = String::from_utf8_lossy(&req[..head_end]).to_string();
        req.drain(..head_end);
        let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
        let close = head.to_ascii_lowercase().contains("connection: close");
        if o.verbose {
            eprintln!("server: request {path} via {ver} (rx seq {}, key updates {})", c.rx.seq, c.key_updates_rx);
        }
        if path == "/big" {
            let f = std::fs::File::open(&o.big)?;
            let len = f.metadata()?.len();
            let h = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nContent-Type: application/octet-stream\r\n\r\n");
            c.send_data(h.as_bytes())?;
            let mut buf = vec![0u8; 16384];
            let mut off = 0u64;
            use std::os::unix::fs::FileExt;
            while off < len {
                let n = f.read_at(&mut buf, off)?;
                if n == 0 { break }
                c.send(record::CT_APPDATA, &buf[..n])?;
                off += n as u64;
            }
        } else {
            let body = format!("hello from {ver}\n");
            let h = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{body}", body.len());
            c.send_data(h.as_bytes())?;
        }
        if close {
            c.close_notify()?;
            return Ok(());
        }
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn plain_conn(fd: RawFd, o: &Opts) -> io::Result<()> {
    let r = (|| {
        let mut req = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            while find(&req, b"\r\n\r\n").is_none() {
                let n = unsafe { libc::read(fd, b.as_mut_ptr().cast(), b.len()) };
                if n <= 0 { return Ok(()) }
                req.extend_from_slice(&b[..n as usize]);
            }
            let e = find(&req, b"\r\n\r\n").unwrap_or(0) + 4;
            let head = String::from_utf8_lossy(&req[..e]).to_string();
            req.drain(..e);
            let path = head.split_whitespace().nth(1).unwrap_or("/");
            if path == "/big" {
                let f = std::fs::File::open(&o.big)?;
                let len = f.metadata()?.len();
                let h = format!("HTTP/1.1 200 OK\r\nContent-Length: {len}\r\n\r\n");
                sys::write_full(fd, h.as_bytes())?;
                use std::os::fd::AsRawFd;
                let mut off: libc::off_t = 0;
                while (off as u64) < len {
                    let n = unsafe { libc::sendfile(fd, f.as_raw_fd(), &mut off, (len - off as u64) as usize) };
                    if n <= 0 { return Err(io::Error::last_os_error()) }
                }
            } else {
                sys::write_full(fd, b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nplain\n")?;
            }
        }
    })();
    unsafe { libc::close(fd) };
    r
}

// ---------------------------------------------------------------- separate-user self tests

/// A handover sent by a process with the wrong uid must be rejected.
fn test_ctl_reject() {
    let (a, b) = sys::seqpacket_pair().expect("pair");
    sys::set_passcred(a).expect("passcred");
    println!("SO_PEERCRED on a socketpair end (created by uid {}): {:?}", unsafe { libc::getuid() },
        sys::peercred(a).map(|c| (c.pid, c.uid)));
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        sys::drop_to(sys::KEYD_UID).expect("drop"); // the wrong uid
        let fake = [0u8; sys::HANDOVER_LEN];
        let (x, _y) = sys::seqpacket_pair().expect("pair");
        let _ = sys::send_fds(b, &fake, &[x]);
        std::process::exit(0);
    }
    unsafe { libc::close(b) };
    sys::drop_to(sys::SERVER_UID).expect("drop");
    let mut m = [0u8; sys::HANDOVER_LEN + 16];
    let rx = sys::recv_fds(a, &mut m).expect("recv");
    let cred = rx.cred.expect("cred");
    println!("SO_PEERCRED still says {:?}; SCM_CREDENTIALS says pid {} uid {}",
        sys::peercred(a).map(|c| (c.pid, c.uid)), cred.pid, cred.uid);
    if cred.uid != sys::HELPER_UID {
        println!("server: REJECT handover from uid {} (expected {})", cred.uid, sys::HELPER_UID);
    }
}

/// A process with the server's uid tries to get a signature from keyd.
fn probe_keyd() {
    match sys::connect_abstract(sys::KEYD_ABSTRACT) {
        Err(e) => println!("probe-keyd uid {}: connect failed: {e}", unsafe { libc::getuid() }),
        Ok(fd) => {
            let mut msg = vec![0x20u8; 64];
            msg.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
            msg.extend_from_slice(&[0u8; 32]);
            let s = sys::send_fds(fd, &msg, &[]);
            let mut b = [0u8; 256];
            let r = sys::recv_fds(fd, &mut b).map(|r| r.len);
            println!("probe-keyd uid {}: send {:?}, reply length {:?} (0 = refused/EOF)", unsafe { libc::getuid() }, s.map(|_| ()), r);
        }
    }
}
