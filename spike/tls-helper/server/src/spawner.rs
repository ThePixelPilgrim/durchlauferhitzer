// THROWAWAY spike: the spawner. Started before containment (root) or as its
// own process under the helper uid. Forks + execs one helper per connection,
// optionally keeping a pool of pre-started helpers.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;

use crate::{sys, Opts};

struct Child {
    pid: i32,
    inbox: RawFd,
}

pub struct Spawner {
    helper: CString,
    memfd: RawFd,
    uidswitch: bool,
    env: Vec<CString>,
    pool_size: usize,
    pool: VecDeque<Child>,
    running: HashMap<i32, u64>,
    pub spawned: u64,
}

fn chain_memfd(path: &str) -> RawFd {
    let der = std::fs::read(path).expect("read chain");
    let fd = unsafe { libc::memfd_create(c"chain".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    let mut buf = (der.len() as u32).to_le_bytes().to_vec();
    buf.extend_from_slice(&der);
    unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    fd
}

impl Spawner {
    pub fn new(o: &Opts, uidswitch: bool, accept_uids: &str) -> Spawner {
        let mut env: Vec<CString> = std::env::vars()
            .filter(|(k, _)| k.starts_with("SPIKE_") && k != "SPIKE_ACCEPT_UIDS")
            .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
            .collect();
        env.push(CString::new(format!("SPIKE_ACCEPT_UIDS={accept_uids}")).unwrap_or_default());
        Spawner {
            helper: CString::new(o.helper.clone()).unwrap_or_default(),
            memfd: chain_memfd(&o.chain),
            uidswitch,
            env,
            pool_size: o.pool,
            pool: VecDeque::new(),
            running: HashMap::new(),
            spawned: 0,
        }
    }

    fn spawn(&mut self, inbox_override: Option<RawFd>) -> io::Result<Child> {
        let (a, b) = match inbox_override {
            Some(fd) => (-1, fd),
            None => sys::seqpacket_pair()?,
        };
        let t = CString::new(sys::now_ns().to_string()).unwrap_or_default();
        let argv = [self.helper.as_ptr(), t.as_ptr(), std::ptr::null()];
        let mut envp: Vec<*const libc::c_char> = self.env.iter().map(|c| c.as_ptr()).collect();
        envp.push(std::ptr::null());
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            unsafe {
                if self.uidswitch && sys::drop_to(sys::HELPER_UID).is_err() {
                    libc::_exit(120);
                }
                let k = match sys::connect_abstract(sys::KEYD_ABSTRACT) {
                    Ok(k) => k,
                    Err(_) => libc::_exit(121),
                };
                let hb = libc::fcntl(b, libc::F_DUPFD, 100);
                let hk = libc::fcntl(k, libc::F_DUPFD, 100);
                let hm = libc::fcntl(self.memfd, libc::F_DUPFD, 100);
                libc::dup2(hb, 3);
                libc::dup2(hk, 4);
                libc::dup2(hm, 5);
                libc::syscall(libc::SYS_close_range, 6u32, u32::MAX, 0u32);
                libc::execve(self.helper.as_ptr(), argv.as_ptr(), envp.as_ptr());
                libc::_exit(127);
            }
        }
        if a >= 0 {
            unsafe { libc::close(b) };
        }
        self.spawned += 1;
        Ok(Child { pid, inbox: a })
    }

    fn dispatch(&mut self, client: RawFd, ctl: RawFd) {
        let child = match self.pool.pop_front() {
            Some(c) => Ok(c),
            None => self.spawn(None),
        };
        match child {
            Ok(c) => {
                if let Err(e) = sys::send_fds(c.inbox, b"w", &[client, ctl]) {
                    eprintln!("spawner: send to helper {} failed: {e}", c.pid);
                }
                unsafe { libc::close(c.inbox) };
                self.running.insert(c.pid, sys::now_ns());
            }
            Err(e) => eprintln!("spawner: spawn failed: {e}"),
        }
        unsafe {
            libc::close(client);
            libc::close(ctl);
        }
    }

    fn housekeeping(&mut self) {
        loop {
            let mut st = 0;
            let pid = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            self.running.remove(&pid);
            if libc::WIFSIGNALED(st) {
                eprintln!("spawner: helper {pid} killed by signal {}", libc::WTERMSIG(st));
            } else if libc::WEXITSTATUS(st) != 0 {
                eprintln!("spawner: helper {pid} exited with status {}", libc::WEXITSTATUS(st));
            }
        }
        let now = sys::now_ns();
        for (&pid, &t) in &self.running {
            if now - t > 15_000_000_000 {
                eprintln!("spawner: helper {pid} hung for 15 s, SIGKILL");
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
        while self.pool.len() < self.pool_size {
            match self.spawn(None) {
                Ok(c) => self.pool.push_back(c),
                Err(e) => {
                    eprintln!("spawner: refill failed: {e}");
                    break;
                }
            }
        }
    }

    /// Serve requests from one server socket until it closes.
    pub fn run(&mut self, sock: RawFd) {
        loop {
            self.housekeeping();
            let mut p = libc::pollfd { fd: sock, events: libc::POLLIN, revents: 0 };
            let r = unsafe { libc::poll(&mut p, 1, 1000) };
            if r <= 0 {
                continue;
            }
            let mut b = [0u8; 8];
            match sys::recv_fds(sock, &mut b) {
                Ok(rx) if rx.len > 0 && rx.fds.len() == 2 => self.dispatch(rx.fds[0], rx.fds[1]),
                Ok(rx) if rx.len == 0 => {
                    eprintln!("spawner: server gone, exiting");
                    return;
                }
                Ok(rx) => {
                    for f in rx.fds {
                        unsafe { libc::close(f) };
                    }
                }
                Err(e) => {
                    eprintln!("spawner: recv: {e}");
                    return;
                }
            }
        }
    }
}

pub fn run_root(o: &Opts, sock: RawFd) {
    let mut s = Spawner::new(o, o.uidswitch, if o.uidswitch { "0" } else { "0" });
    eprintln!("spawner: root, uid switch to {}: {}, pool {}", sys::HELPER_UID, o.uidswitch, o.pool);
    s.run(sock);
}

/// The helper spawner as its own long-lived process under the helper uid.
pub fn run_unpriv(o: &Opts) {
    let uid = unsafe { libc::getuid() };
    let lfd = sys::listen_abstract(sys::HSPAWN_ABSTRACT).expect("listen");
    let mut s = Spawner::new(o, false, &uid.to_string());
    eprintln!("hspawn: uid {uid}, no uid switch, pool {}", o.pool);
    loop {
        let c = unsafe { libc::accept4(lfd, std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC) };
        if c < 0 {
            continue;
        }
        match sys::peercred(c) {
            Ok(pc) if pc.uid == sys::SERVER_UID => {
                eprintln!("hspawn: server pid {} uid {} connected", pc.pid, pc.uid);
                s.run(c);
            }
            Ok(pc) => eprintln!("hspawn: REJECT peer uid {} pid {}", pc.uid, pc.pid),
            Err(e) => eprintln!("hspawn: peercred: {e}"),
        }
        unsafe { libc::close(c) };
    }
}

/// A process with the key uid (wrong) hands a helper its inbox.
pub fn test_helper_reject(o: &Opts) {
    let (c0, c1) = sys::seqpacket_pair().expect("pair");
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        sys::drop_to(sys::KEYD_UID).expect("drop");
        let (x, y) = sys::seqpacket_pair().expect("pair");
        let _ = sys::send_fds(c1, b"y", &[y]);
        let (cl, ct) = sys::seqpacket_pair().expect("pair");
        let _ = sys::send_fds(x, b"w", &[cl, ct]);
        std::thread::sleep(std::time::Duration::from_secs(2));
        std::process::exit(0);
    }
    let mut b = [0u8; 4];
    let rx = sys::recv_fds(c0, &mut b).expect("recv");
    let mut s = Spawner::new(o, true, "0");
    let child = s.spawn(Some(rx.fds[0])).expect("spawn");
    let mut st = 0;
    unsafe { libc::waitpid(child.pid, &mut st, 0) };
    println!("helper exit status {} (inbox created by uid {})", libc::WEXITSTATUS(st), sys::KEYD_UID);
}
