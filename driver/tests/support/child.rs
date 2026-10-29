//! Running the built binary (`CARGO_BIN_EXE_durchlauferhitzer`) for the
//! CLI and startup tests: a config in a temp file, a free loopback port,
//! the child's setup before `exec` (closed descriptors, a listener as fd 3,
//! a lowered `RLIMIT_NOFILE`, a non-root user when the tests run as root),
//! a guard that kills a running server (SIGKILL) when dropped, so no
//! test leaves a child behind, and for the shutdown tests: signals sent
//! with the shell's `kill`, the server's stderr read line by line while
//! it runs ([`Lines`]), and a bounded wait for its exit. [`run_program`]
//! runs another program with the same setup, as a control.
//!
//! The setup before `exec` needs `pre_exec`, which is `unsafe`: it lives in
//! the one module [`pre_exec`] below, the only `unsafe` code of the test
//! crate (controller ruling R2), and calls only async-signal-safe
//! functions (`close`, `dup2`, `setrlimit`, and `setresuid`, `prctl`,
//! `setgroups`, `setresgid`, `capget` and `capset`, see there).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    dead_code,
    reason = "test helpers outside #[test] functions; each test binary uses part"
)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// How long any single wait may take before the test fails.
pub const PATIENCE: Duration = Duration::from_secs(5);

/// The usage text the binary prints for bad arguments.
pub const USAGE: &str =
    "usage: durchlauferhitzer <config-path>\n       durchlauferhitzer --check <config-path>\n";

/// The uid and gid the tests run the server as when they themselves run
/// as root and the case needs a non-root server (`nobody`).
pub const NOBODY: u32 = 65534;

pub fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_durchlauferhitzer")
}

/// Whether this test process runs as root.
pub fn is_root() -> bool {
    durchlauferhitzer::sys::user::euid() == 0
}

/// A directory under the system temp dir, removed on drop; readable by
/// everyone, so a server running as `nobody` can read its config.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dlh-cli-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        set_mode(&path, 0o755);
        TempDir(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `text` to `name`, readable by everyone.
    pub fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        set_mode(&path, 0o644);
        path
    }

    /// A directory `name`, readable by everyone.
    pub fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).unwrap();
        set_mode(&path, 0o755);
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A loopback port that was free a moment ago (bound, read, released).
///
/// Another process may take it before the server binds it: a known, rare
/// race, left as it is (a retry would have to rewrite the test's config).
/// [`Running::start`] names it when the server's bind fails that way.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A config with one listener and one host whose `/` route is `route`
/// (the inside of the `path` block), plus `extra` top-level lines.
pub fn config(listen: &str, extra: &str, route: &str) -> String {
    format!("listen {listen};\n{extra}\nhost example.test {{\n  path / {{ {route} }}\n}}\n")
}

/// A proxy route to an address nothing listens on (the tests that use it
/// never send a request there).
pub const PROXY_ROUTE: &str =
    "proxy 127.0.0.1:9; max_request_body 1MiB; max_response_body unlimited;";

/// How the child is set up before `exec`.
#[derive(Default)]
pub struct Setup {
    /// Descriptors closed in the child (after std set up its stdio).
    pub close: Vec<RawFd>,
    /// A descriptor that becomes the child's fd 3, inheritable (systemd's
    /// first socket), with `LISTEN_PID` (the child's pid) and
    /// `LISTEN_FDS=1` set through `sh -c '... exec'`.
    pub listen_fd: Option<OwnedFd>,
    /// A descriptor that becomes the child's fd 3, inheritable, without
    /// any systemd variable: a stray descriptor the server inherits.
    pub stray_fd: Option<OwnedFd>,
    /// The `LISTEN_PID` value instead of the child's pid.
    pub listen_pid: Option<String>,
    /// `RLIMIT_NOFILE` (soft, hard) in the child.
    pub nofile: Option<(u64, u64)>,
    /// Run the child as uid/gid [`NOBODY`] (only possible as root).
    pub as_nobody: bool,
    /// Only the effective uid set to this before `exec` (the real uid
    /// stays; `exec` copies the effective into the saved one).
    pub effective_uid: Option<u32>,
    /// A seccomp filter installed before `exec`.
    pub filter: Filter,
    /// Capabilities the child gets before `exec` (only possible as root).
    pub caps: Caps,
}

/// Capabilities for the child (OA2: the server clears them all).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Caps {
    /// What it inherits from the test process.
    #[default]
    Inherited,
    /// `CAP_NET_BIND_SERVICE` added to its inheritable set; it stays
    /// root. The privilege drop (`setuid`) leaves that set alone.
    Inheritable,
    /// Run as uid/gid [`NOBODY`] with `CAP_NET_BIND_SERVICE` ambient
    /// (and so permitted and effective after `exec`), as systemd's
    /// `AmbientCapabilities=` gives a non-root service. Instead of
    /// [`Setup::as_nobody`].
    AmbientAsNobody,
}

/// `CAP_NET_BIND_SERVICE` (`linux/capability.h`).
pub const CAP_NET_BIND_SERVICE: u32 = 10;

/// A seccomp filter for the child (owner decision 2026-10-29, O3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Filter {
    /// No filter.
    #[default]
    None,
    /// `openat2` fails with `ENOSYS`, as on a kernel without it; every
    /// other system call passes.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    Openat2Enosys,
}

impl Setup {
    /// A setup that runs the server as non-root: as `nobody` when the
    /// tests run as root, as the current user otherwise.
    pub fn non_root() -> Setup {
        Setup {
            as_nobody: is_root(),
            ..Setup::default()
        }
    }
}

/// The command for `args` with `setup` applied; stdout and stderr piped,
/// stdin null.
pub fn command(args: &[&str], setup: Setup) -> (Command, Vec<OwnedFd>) {
    let cmd = match &setup.listen_fd {
        None => {
            let mut cmd = Command::new(exe());
            cmd.args(args);
            cmd
        }
        Some(_) => {
            // `$$` is the shell's pid, which `exec` keeps for the server.
            let pid = setup.listen_pid.as_deref().unwrap_or("$$");
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(format!(
                    "LISTEN_PID={pid} LISTEN_FDS=1 LISTEN_FDNAMES=web exec \"$0\" \"$@\""
                ))
                .arg(exe())
                .args(args);
            cmd
        }
    };
    prepare(cmd, Path::new(exe()), setup)
}

/// `setup` applied to `cmd`, which runs `program`; stdout and stderr
/// piped, stdin null. The descriptors to keep open until the spawn come
/// with it.
fn prepare(mut cmd: Command, program: &Path, setup: Setup) -> (Command, Vec<OwnedFd>) {
    cmd.env_remove("LISTEN_PID")
        .env_remove("LISTEN_FDS")
        .env_remove("LISTEN_FDNAMES")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if setup.as_nobody || setup.effective_uid.is_some() {
        assert_reachable_by_others(program);
    }
    if setup.as_nobody {
        cmd.uid(NOBODY).gid(NOBODY);
    }
    let mut keep = Vec::new();
    if let Some(fd) = setup.listen_fd.or(setup.stray_fd) {
        // `dup2(3, 3)` would keep close-on-exec set: make sure the source
        // is another descriptor (a clone gets the lowest free number,
        // which cannot be 3 while the original holds it).
        let src = if fd.as_raw_fd() == 3 {
            let clone = fd.try_clone().unwrap();
            keep.push(fd);
            clone
        } else {
            fd
        };
        pre_exec::dup_to(&mut cmd, src.as_raw_fd(), 3);
        // Kept open in the parent until the child is spawned.
        keep.push(src);
    }
    if !setup.close.is_empty() {
        pre_exec::close_fds(&mut cmd, setup.close);
    }
    if let Some(uid) = setup.effective_uid {
        pre_exec::set_effective_uid(&mut cmd, uid);
    }
    if let Some((soft, hard)) = setup.nofile {
        pre_exec::limit_nofile(&mut cmd, soft, hard);
    }
    match setup.caps {
        Caps::Inherited => {}
        Caps::Inheritable => pre_exec::inheritable_cap(&mut cmd, CAP_NET_BIND_SERVICE),
        Caps::AmbientAsNobody => {
            assert!(!setup.as_nobody, "AmbientAsNobody sets the ids itself");
            pre_exec::nobody_with_ambient_cap(&mut cmd, NOBODY, CAP_NET_BIND_SERVICE);
        }
    }
    match setup.filter {
        Filter::None => {}
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        Filter::Openat2Enosys => pre_exec::openat2_enosys(&mut cmd),
    }
    (cmd, keep)
}

/// Fails the test with an explanation unless a user other than the owner
/// (`nobody`) can execute `exe`: every directory above it must be
/// searchable by others (`o+x`), and the file executable by them. A
/// target dir under a private home directory is not; build into one
/// others can reach (`CARGO_TARGET_DIR`, e.g. under `/tmp`), or make the
/// directories searchable (CI runs `chmod a+x "$HOME"`).
fn assert_reachable_by_others(exe: &Path) {
    use std::os::unix::fs::PermissionsExt;
    const OTHERS_X: u32 = 0o001;
    for path in exe.ancestors() {
        if path.as_os_str().is_empty() {
            continue;
        }
        let mode = fs::metadata(path).unwrap().permissions().mode();
        assert!(
            mode & OTHERS_X != 0,
            "the server is started as nobody here, but it cannot reach {}: {} has mode {:o}, \
             without o+x. Build into a target dir others can reach (CARGO_TARGET_DIR, \
             e.g. under /tmp) or make the directories above it searchable (chmod o+x)",
            exe.display(),
            path.display(),
            mode & 0o7777
        );
    }
}

/// Runs the binary to completion with `args` and `setup`.
pub fn run(args: &[&str], setup: Setup) -> Output {
    let (mut cmd, _keep) = command(args, setup);
    let child = cmd.spawn().unwrap();
    wait_bounded(child)
}

/// Runs `program` (not the server) to completion with `args` and `setup`:
/// a control that shows what the setup gives a child. A systemd socket
/// (`listen_fd`) is the server's alone.
pub fn run_program(program: &str, args: &[&str], setup: Setup) -> Output {
    assert!(
        setup.listen_fd.is_none(),
        "a systemd socket is for the server"
    );
    let mut cmd = Command::new(program);
    cmd.args(args);
    let (mut cmd, _keep) = prepare(cmd, Path::new(program), setup);
    let child = cmd.spawn().unwrap();
    wait_bounded(child)
}

/// Waits for `child` to exit, at most [`PATIENCE`]; kills it after that
/// and fails the test.
fn wait_bounded(child: Child) -> Output {
    let mut guard = Running { child };
    let deadline = Instant::now() + PATIENCE;
    loop {
        if guard.child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "the server did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut out) = guard.child.stdout.take() {
        out.read_to_end(&mut stdout).unwrap();
    }
    if let Some(mut err) = guard.child.stderr.take() {
        err.read_to_end(&mut stderr).unwrap();
    }
    let status = guard.child.wait().unwrap();
    Output {
        status,
        stdout,
        stderr,
    }
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Asserts the exit code, printing the child's output when it differs.
pub fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        text(&output.stdout),
        text(&output.stderr)
    );
}

/// A server process; killed (SIGKILL) and reaped when dropped.
pub struct Running {
    pub child: Child,
}

impl Running {
    /// Starts the binary with `args` and `setup`, and waits until `addr`
    /// accepts a connection (or the child exits, which fails the test).
    pub fn start(args: &[&str], setup: Setup, addr: SocketAddr) -> Running {
        let (mut cmd, keep) = command(args, setup);
        let running = Running {
            child: cmd.spawn().unwrap(),
        };
        drop(keep);
        running.wait_ready(addr)
    }

    fn wait_ready(mut self, addr: SocketAddr) -> Running {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut stderr = String::new();
                if let Some(mut err) = self.child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr);
                }
                // The race of `free_port`: another process took the port.
                let race = if stderr.contains("Address already in use") {
                    " (the port from `free_port` was taken before the server \
                     bound it, a known rare race: rerun the test)"
                } else {
                    ""
                };
                panic!("the server exited early ({status}){race}: {stderr}");
            }
            if TcpStream::connect(addr).is_ok() {
                return self;
            }
            assert!(Instant::now() < deadline, "the server never listened");
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Takes the server's stderr and reads it line by line in a thread,
    /// so the test can wait for a line while the server runs.
    pub fn lines(&mut self) -> Lines {
        let stderr = self.child.stderr.take().expect("stderr already taken");
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { return };
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        Lines {
            receiver,
            seen: Vec::new(),
        }
    }

    /// Sends `signal` (`TERM`, `INT`, `HUP`, ...) to the server; returns
    /// once it is sent.
    pub fn signal(&self, signal: &str) {
        signal_pid(self.pid(), signal);
    }

    /// Waits until the server has exited, at most `limit` (it is killed
    /// and the test fails after that), and returns its status.
    pub fn wait_exit(&mut self, limit: Duration) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the server did not exit within {limit:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Kills the server and returns its stderr.
    pub fn kill(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let mut stderr = Vec::new();
        if let Some(mut err) = self.child.stderr.take() {
            err.read_to_end(&mut stderr).unwrap();
        }
        text(&stderr)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sends `signal` to `pid` with the shell's `kill` builtin (no `unsafe`
/// in the tests, R2).
pub fn signal_pid(pid: u32, signal: &str) {
    let status = Command::new("sh")
        .arg("-c")
        .arg("kill -s \"$1\" \"$2\"")
        .arg("sh")
        .arg(signal)
        .arg(pid.to_string())
        .status()
        .unwrap();
    assert!(status.success(), "kill -s {signal} {pid}: {status}");
}

/// The server's stderr, line by line (without the `\n`), as it comes.
pub struct Lines {
    receiver: Receiver<String>,
    /// Every line received so far.
    seen: Vec<String>,
}

impl Lines {
    /// Waits for `line` (at most [`PATIENCE`]); fails with what came.
    pub fn wait_for(&mut self, line: &str) {
        if self.seen.iter().any(|seen| seen == line) {
            return;
        }
        let deadline = Instant::now() + PATIENCE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(left) {
                Ok(got) => {
                    let found = got == line;
                    self.seen.push(got);
                    if found {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                    panic!("no line {line:?}; got {:?}", self.seen)
                }
            }
        }
    }

    /// Every line the server wrote, once its stderr has closed (it has
    /// exited); at most [`PATIENCE`].
    pub fn all(mut self) -> Vec<String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.receiver.recv_timeout(left) {
                Ok(got) => self.seen.push(got),
                Err(RecvTimeoutError::Disconnected) => return self.seen,
                Err(RecvTimeoutError::Timeout) => panic!("stderr never closed: {:?}", self.seen),
            }
        }
    }
}

/// Sends a `GET /` for `example.test` and returns the whole response
/// (the request asks for `Connection: close`).
pub fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    text(&response)
}

/// The thread directories `/proc/<pid>/task/*` once there are `count` of
/// them: the listener accepts (the readiness check) before the disk
/// workers are started, so they may still be coming.
pub fn threads(pid: u32, count: usize) -> Vec<PathBuf> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        let tasks: Vec<PathBuf> = fs::read_dir(format!("/proc/{pid}/task"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        if tasks.len() >= count || Instant::now() >= deadline {
            return tasks;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// How many descriptors of this process, from 3 on and not in `except`,
/// a child inherits: those without `O_CLOEXEC` in `/proc/self/fdinfo`
/// (cargo's jobserver pipes or a CI runner's descriptors, say; std opens
/// everything close-on-exec). The server counts them in its budget (O5).
pub fn inheritable_fds(except: &[RawFd]) -> u64 {
    const O_CLOEXEC_OCTAL: u32 = 0o2_000_000;
    let mut count = 0;
    for entry in fs::read_dir("/proc/self/fdinfo").unwrap() {
        let entry = entry.unwrap();
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<RawFd>() else {
            continue;
        };
        if fd < 3 || except.contains(&fd) {
            continue;
        }
        // The directory being read is gone by the time this is read, so a
        // vanished entry is skipped.
        let Ok(info) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let flags = info
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|flags| u32::from_str_radix(flags.trim(), 8).ok())
            .unwrap();
        if flags & O_CLOEXEC_OCTAL == 0 {
            count += 1;
        }
    }
    count
}

/// The target of `/proc/<pid>/fd/<fd>`, if it is open.
pub fn fd_target(pid: u32, fd: RawFd) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/fd/{fd}"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// The status of a finished child as a code, for messages.
pub fn code(status: ExitStatus) -> Option<i32> {
    status.code()
}

/// The only `unsafe` code of the test crate (R2): closures run in the
/// child between `fork` and `exec`, where only async-signal-safe
/// functions may be called. They allocate nothing and touch no locks;
/// every value they use is copied in before the fork.
#[allow(unsafe_code)]
mod pre_exec {
    use std::io;
    use std::os::fd::RawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    /// Closes `fds` in the child.
    pub fn close_fds(cmd: &mut Command, fds: Vec<RawFd>) {
        let hook = move || {
            for &fd in &fds {
                // SAFETY: `close` is async-signal-safe; the child owns its
                // copies of the descriptors and nothing in it uses them
                // before `exec`. Iterating the Vec only reads memory that
                // was allocated before the fork.
                unsafe { libc::close(fd) };
            }
            Ok(())
        };
        // SAFETY: the hook calls only `close` (async-signal-safe) and
        // neither allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }

    /// Makes `src` the child's `dst`, without close-on-exec (`src` must
    /// differ from `dst`, or the flag would stay).
    pub fn dup_to(cmd: &mut Command, src: RawFd, dst: RawFd) {
        assert_ne!(src, dst);
        let hook = move || {
            // SAFETY: `dup2` is async-signal-safe and takes no pointers.
            if unsafe { libc::dup2(src, dst) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: the hook calls only `dup2` (async-signal-safe) and reads
        // errno; it neither allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }

    /// Sets only the child's effective uid (`setresuid(-1, uid, -1)`).
    /// Not in R2's list: `setresuid` is a single system call in a
    /// single-threaded child (glibc's cross-thread broadcast has no other
    /// thread to reach), takes no lock and allocates nothing.
    pub fn set_effective_uid(cmd: &mut Command, uid: u32) {
        let hook = move || {
            let keep = libc::uid_t::MAX;
            // SAFETY: `setresuid` takes no pointers; in the forked child
            // only this thread exists, so it is one system call.
            if unsafe { libc::setresuid(keep, uid, keep) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: the hook calls only `setresuid` and reads errno; it
        // neither allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }

    /// `_LINUX_CAPABILITY_VERSION_3`: a header (version, pid 0 for the
    /// calling thread) and two data words of (effective, permitted,
    /// inheritable), low then high 32 bits.
    const CAP_VERSION_3: u32 = 0x2008_0522;

    /// Reads the calling thread's capability words (`capget`).
    fn capget(data: &mut [u32; 6]) -> io::Result<()> {
        let mut header: [u32; 2] = [CAP_VERSION_3, 0];
        // SAFETY: `header` is a version-3 header and `data` the two data
        // structs (three `u32`s each) that version fills; both live
        // through the call.
        let ret =
            unsafe { libc::syscall(libc::SYS_capget, header.as_mut_ptr(), data.as_mut_ptr()) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sets the calling thread's capability words (`capset`).
    fn capset(data: &[u32; 6]) -> io::Result<()> {
        let header: [u32; 2] = [CAP_VERSION_3, 0];
        // SAFETY: `header` is a version-3 header and `data` the two data
        // structs that version takes; the kernel only reads them.
        let ret = unsafe { libc::syscall(libc::SYS_capset, header.as_ptr(), data.as_ptr()) };
        if ret == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Adds capability `cap` (< 32) to the root child's inheritable set
    /// (allowed: it is in the permitted set). Not in R2's list: `capget`
    /// and `capset` are single system calls that take no lock and
    /// allocate nothing.
    pub fn inheritable_cap(cmd: &mut Command, cap: u32) {
        let hook = move || {
            let mut data = [0u32; 6];
            capget(&mut data)?;
            data[2] |= 1 << cap;
            capset(&data)
        };
        // SAFETY: the hook calls only `capget` and `capset` and reads
        // errno; it neither allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }

    /// Runs the root child as uid and gid `id` with capability `cap`
    /// (< 32) ambient: `PR_SET_KEEPCAPS` keeps the permitted set through
    /// `setresuid`, then the permitted and inheritable sets become `cap`
    /// alone and `cap` is raised into the ambient set, so after `exec` it
    /// is permitted, effective and ambient. Not in R2's list: `prctl`,
    /// `setgroups`, `setresgid`, `setresuid`, `capset` are single system
    /// calls in a single-threaded child (glibc's cross-thread broadcast
    /// has no other thread to reach) that take no lock and allocate
    /// nothing.
    pub fn nobody_with_ambient_cap(cmd: &mut Command, id: u32, cap: u32) {
        let hook = move || {
            let zero: libc::c_ulong = 0;
            let check = |ret: libc::c_int| {
                if ret == -1 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            };
            // SAFETY: `prctl(PR_SET_KEEPCAPS, 1)` takes no pointers.
            check(unsafe {
                libc::prctl(libc::PR_SET_KEEPCAPS, 1 as libc::c_ulong, zero, zero, zero)
            })?;
            let groups = [id];
            // SAFETY: `groups` is readable for the one entry passed.
            check(unsafe { libc::setgroups(1, groups.as_ptr()) })?;
            // SAFETY: `setresgid` takes no pointers.
            check(unsafe { libc::setresgid(id, id, id) })?;
            // SAFETY: `setresuid` takes no pointers.
            check(unsafe { libc::setresuid(id, id, id) })?;
            let bit = 1 << cap;
            capset(&[0, bit, bit, 0, 0, 0])?;
            // SAFETY: `prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, cap)`
            // takes no pointers.
            check(unsafe {
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::c_ulong::from(libc::PR_CAP_AMBIENT_RAISE.unsigned_abs()),
                    libc::c_ulong::from(cap),
                    zero,
                    zero,
                )
            })
        };
        // SAFETY: the hook calls only the system calls listed above and
        // reads errno; it neither allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }

    /// Sets the child's `RLIMIT_NOFILE` to `soft`/`hard`.
    pub fn limit_nofile(cmd: &mut Command, soft: u64, hard: u64) {
        let hook = move || {
            let limit = libc::rlimit {
                rlim_cur: soft,
                rlim_max: hard,
            };
            // SAFETY: `setrlimit` is async-signal-safe; `limit` is a valid
            // `rlimit` the kernel only reads.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: the hook calls only `setrlimit` (async-signal-safe) and
        // reads errno; it neither allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }

    /// The audit architecture of this target (`AUDIT_ARCH_*` in
    /// `linux/audit.h`: the ELF machine, 64-bit, little-endian).
    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;

    /// Installs a seccomp filter in the child that fails `openat2` with
    /// `ENOSYS` (owner decision 2026-10-29, O3). It checks the
    /// architecture first, so only this target's `openat2` number is
    /// matched; every other system call is allowed. `PR_SET_NO_NEW_PRIVS`
    /// comes first, so no privilege is needed. Not in R2's list: `prctl`
    /// is a single system call that takes no lock and allocates nothing.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub fn openat2_enosys(cmd: &mut Command) {
        let stmt = |code: u32, k: u32| libc::sock_filter {
            code: u16::try_from(code).unwrap(),
            jt: 0,
            jf: 0,
            k,
        };
        let jump_eq = |k: u32, jt: u8, jf: u8| libc::sock_filter {
            code: u16::try_from(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K).unwrap(),
            jt,
            jf,
            k,
        };
        let load = libc::BPF_LD | libc::BPF_W | libc::BPF_ABS;
        // `seccomp_data`: `nr` at offset 0, `arch` at offset 4.
        let openat2 = u32::try_from(libc::SYS_openat2).unwrap();
        let enosys = u32::try_from(libc::ENOSYS).unwrap();
        // Built before the fork; the hook only reads it.
        let program: [libc::sock_filter; 6] = [
            stmt(load, 4),
            jump_eq(AUDIT_ARCH, 0, 3),
            stmt(load, 0),
            jump_eq(openat2, 0, 1),
            stmt(
                libc::BPF_RET | libc::BPF_K,
                libc::SECCOMP_RET_ERRNO | enosys,
            ),
            stmt(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW),
        ];
        let hook = move || {
            // SAFETY: `prctl(PR_SET_NO_NEW_PRIVS)` takes no pointers.
            if unsafe {
                libc::prctl(
                    libc::PR_SET_NO_NEW_PRIVS,
                    1 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                    0 as libc::c_ulong,
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            let fprog = libc::sock_fprog {
                len: 6,
                filter: program.as_ptr().cast_mut(),
            };
            // SAFETY: `fprog` points to `program`, six valid instructions
            // that live in this closure for the whole call; the kernel
            // only reads them (it copies the filter).
            let ret = unsafe {
                libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::c_ulong::from(libc::SECCOMP_MODE_FILTER),
                    &raw const fprog,
                )
            };
            if ret == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        // SAFETY: the hook calls only `prctl` and reads errno; it neither
        // allocates nor locks.
        unsafe { cmd.pre_exec(hook) };
    }
}
