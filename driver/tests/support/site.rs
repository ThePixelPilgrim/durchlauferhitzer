//! The restart tests' scaffolding (`restart.rs`): the root and
//! `tcp_migrate_req` checks, one test at a time ([`serial`]), a [`Site`]
//! (its own binary copy, configs, pid file and log, the reload helper run
//! against it), and process helpers read from `/proc`.

use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use durchlauferhitzer::sys::fd::pipe;

use super::child::{
    self, Containment, Running, Setup, TempDir, command, config, exe, free_port, is_root, text,
};

pub const REQUIRE_ROOT: &str = "DURCHLAUFERHITZER_REQUIRE_ROOT";
pub const REQUIRE_MIGRATE: &str = "DURCHLAUFERHITZER_REQUIRE_MIGRATE";
pub const MIGRATE: &str = "/proc/sys/net/ipv4/tcp_migrate_req";

/// The reload helper, run with `sh`.
pub const HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../contrib/durchlauferhitzer-reload"
);

/// The OpenRC files and their end-to-end test, checked with `sh -n`.
pub const OPENRC: [&str; 3] = [
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../contrib/openrc/durchlauferhitzer"
    ),
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../contrib/openrc/durchlauferhitzer.confd"
    ),
    concat!(env!("CARGO_MANIFEST_DIR"), "/../contrib/test-openrc.sh"),
];

/// One test at a time: the sysctl is global, and a binary copied while
/// another test forks would be busy (`ETXTBSY`) for its `exec`.
pub fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub fn required(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| v == "1")
}

/// Whether the restart tests can run here: as root, with
/// `tcp_migrate_req` set to 1 (by this test, as the helper would).
pub fn can_restart() -> bool {
    if !is_root() {
        assert!(
            !required(REQUIRE_ROOT),
            "not root, but {REQUIRE_ROOT}=1 requires it"
        );
        eprintln!("skipped: the restart tests need root");
        return false;
    }
    set_migrate(Migration::On)
}

/// The value of `tcp_migrate_req`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Migration {
    /// 1: a closing listener's queue moves to another in its group.
    On,
    /// 0: it is reset.
    Off,
}

/// Sets `tcp_migrate_req`; whether it took (a failure skips the test, or
/// fails it with `DURCHLAUFERHITZER_REQUIRE_MIGRATE=1`).
pub fn set_migrate(migration: Migration) -> bool {
    let value = match migration {
        Migration::On => "1",
        Migration::Off => "0",
    };
    let set = fs::write(MIGRATE, value)
        .and_then(|()| fs::read_to_string(MIGRATE))
        .map(|now| now.trim() == value);
    match set {
        Ok(true) => true,
        other => {
            assert!(
                !required(REQUIRE_MIGRATE),
                "{MIGRATE} cannot be set to {value} ({other:?}), but {REQUIRE_MIGRATE}=1 requires it"
            );
            eprintln!("skipped: {MIGRATE} cannot be set to {value}: {other:?}");
            false
        }
    }
}

pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A site in a temp dir: its own copy of the binary, a pid file, a log,
/// one document root per answer (`<answer>/a.txt` holds `<answer>\n`).
/// Every process running its binary is killed when it is dropped.
pub struct Site {
    pub dir: TempDir,
    /// The address the tests serve on (a free loopback port).
    pub addr: SocketAddr,
    pub bin: PathBuf,
    pub pidfile: PathBuf,
    pub log: PathBuf,
}

impl Site {
    pub fn new() -> Site {
        let dir = TempDir::new();
        let bin_dir = dir.dir("bin");
        let bin = bin_dir.join("durchlauferhitzer");
        install(&bin);
        let bin = fs::canonicalize(bin).unwrap();
        let pidfile = dir.path().join("durchlauferhitzer.pid");
        let log = dir.path().join("durchlauferhitzer.log");
        Site {
            dir,
            addr: loopback(free_port()),
            bin,
            pidfile,
            log,
        }
    }

    /// The config `name.conf`: `listen` on each address, `user nobody`,
    /// a per-IP limit above the tests' connection counts, and `/` served
    /// from `root` (which holds `a.txt` = `root\n` unless it is absolute,
    /// then used as written).
    pub fn config(&self, name: &str, listen: &[SocketAddr], root: &str) -> PathBuf {
        let root = if root.starts_with('/') {
            root.to_owned()
        } else {
            let www = self.dir.dir(root);
            self.dir
                .file(&format!("{root}/a.txt"), &format!("{root}\n"));
            www.display().to_string()
        };
        let listen: Vec<String> = listen.iter().map(ToString::to_string).collect();
        let text = config(
            &listen.join(" "),
            "user nobody;\nlimits { max_connections_per_ip 1024; }",
            &format!("static {root};"),
        );
        self.dir
            .config(&format!("{name}.conf"), &text, Containment::Switch)
    }

    /// Starts the server on `conf` as root (it drops to `nobody`), its
    /// output appended to the log, waits until the site's address
    /// answers, and writes the pid file, as OpenRC's `start` would.
    pub fn start(&self, conf: &Path) -> Running {
        let log = || {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.log)
                .unwrap()
        };
        let child = Command::new(&self.bin)
            .arg(conf)
            .stdin(Stdio::null())
            .stdout(log())
            .stderr(log())
            .spawn()
            .unwrap_or_else(|err| panic!("{}: {err}", self.bin.display()));
        let mut server = Running { child };
        let deadline = Instant::now() + child::PATIENCE;
        while TcpStream::connect(self.addr).is_err() {
            if let Some(status) = server.child.try_wait().unwrap() {
                panic!("the server exited ({status}): {}", self.log_text());
            }
            assert!(Instant::now() < deadline, "the server never listened");
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(&self.pidfile, format!("{}\n", server.pid())).unwrap();
        server
    }

    /// Starts the server on `conf` with `--ready-fd 3` beside a running
    /// one (as root, as the helper would, but without it) and waits for
    /// `ready\n` and EOF on the pipe.
    pub fn start_ready(&self, conf: &Path) -> Running {
        let (read, write) = pipe().unwrap();
        let setup = Setup {
            stray_fd: Some(write),
            ..Setup::default()
        };
        let conf = conf.to_str().unwrap();
        let (mut cmd, keep) = command(&["--ready-fd", "3", conf], setup);
        let server = Running {
            child: cmd.spawn().unwrap(),
        };
        // The parent's copies of the write end go: EOF comes from the server.
        drop(keep);
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = File::from(read).read_to_end(&mut bytes);
            let _ = sender.send(result.map(|_| text(&bytes)));
        });
        let written = receiver
            .recv_timeout(child::PATIENCE)
            .expect("no EOF on the ready pipe")
            .unwrap();
        assert_eq!(written, "ready\n", "{}", self.log_text());
        server
    }

    /// Replaces the binary with a fresh copy (a new file renamed over it),
    /// so a running process's `/proc/<pid>/exe` reads ` (deleted)`.
    pub fn replace_binary(&self) {
        let fresh = self.bin.with_extension("new");
        install(&fresh);
        fs::rename(&fresh, &self.bin).unwrap();
    }

    /// Runs the reload helper with `conf` and a 10 s readiness timeout.
    pub fn reload(&self, conf: &Path) -> Output {
        self.reload_within(conf, "10")
    }

    /// Runs the reload helper with `conf` and a readiness timeout of
    /// `timeout` seconds; its temp dirs go to [`Site::tmp`].
    pub fn reload_within(&self, conf: &Path, timeout: &str) -> Output {
        bounded(self.helper(conf, &[timeout]))
    }

    /// Starts the reload helper with `conf` and `more` arguments (timeout,
    /// grace) and returns at once, its output piped.
    pub fn spawn_reload(&self, conf: &Path, more: &[&str]) -> Child {
        self.helper(conf, more)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// The helper's command line for `conf`, then `more`.
    fn helper(&self, conf: &Path, more: &[&str]) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg(HELPER)
            .args([&self.bin, conf, &self.pidfile, &self.log])
            .args(more)
            .env("TMPDIR", self.tmp());
        cmd
    }

    /// The helper's `TMPDIR`.
    pub fn tmp(&self) -> PathBuf {
        self.dir.dir("tmp")
    }

    /// Runs the helper as [`Site::reload`] does, with `file` open on fds 5
    /// and 9 (as a caller such as `openrc-run` may leave descriptors open).
    pub fn reload_inheriting(&self, conf: &Path, file: &Path) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(r#"exec 5<"$1" 9<"$1"; shift; exec sh "$@""#)
            .arg("sh")
            .arg(file)
            .arg(HELPER)
            .args([&self.bin, conf, &self.pidfile, &self.log])
            .arg("10");
        bounded(cmd)
    }

    /// Runs the OpenRC script's `stop_post` (sourced by `sh`, with stub
    /// `ebegin`/`eend`) for this site, with `retry`.
    pub fn stop_post(&self, retry: &str) -> Output {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(
                r#"ebegin() { :; }; eend() { :; }; . "$1"; command=$2 pidfile=$3 retry=$4; stop_post"#,
            )
            .arg("sh")
            .arg(OPENRC[0])
            .args([&self.bin, &self.pidfile])
            .arg(retry);
        bounded(cmd)
    }

    /// The pids `<pidfile>.draining` lists (none without the file).
    /// Each line is `<pid> <start time>`; while the process still runs,
    /// the start time must be its own (one that finished draining may
    /// be gone by the time this reads it).
    pub fn draining(&self) -> Vec<u32> {
        fs::read_to_string(self.draining_file())
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let (pid, start) = line.split_once(' ').unwrap();
                let pid = pid.parse().unwrap();
                if let Some(now) = start_time(pid) {
                    assert_eq!(start, now, "{line}");
                }
                pid
            })
            .collect()
    }

    pub fn draining_file(&self) -> PathBuf {
        self.dir.path().join("durchlauferhitzer.pid.draining")
    }

    /// The pid the pid file names.
    pub fn pid(&self) -> u32 {
        fs::read_to_string(&self.pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    pub fn log_text(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Every live process running this site's binary (the current file or
    /// a replaced one).
    pub fn processes(&self) -> Vec<u32> {
        let bin = self.bin.display().to_string();
        let deleted = format!("{bin} (deleted)");
        let mut pids = Vec::new();
        for entry in fs::read_dir("/proc").unwrap() {
            let Ok(entry) = entry else { continue };
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            // A zombie's `exe` cannot be read: it is not running.
            let Ok(exe) = fs::read_link(entry.path().join("exe")) else {
                continue;
            };
            let exe = exe.display().to_string();
            if exe == bin || exe == deleted {
                pids.push(pid);
            }
        }
        pids.sort_unstable();
        pids
    }

    pub fn assert_reload_ok(&self, output: &Output) {
        assert_eq!(
            output.status.code(),
            Some(0),
            "reload: {:?}\nstdout: {}\nstderr: {}\nlog: {}",
            output.status,
            text(&output.stdout),
            text(&output.stderr),
            self.log_text()
        );
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        for pid in self.processes() {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Copies the built binary to `path`, executable by everyone.
pub fn install(path: &Path) {
    fs::copy(exe(), path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Runs the helper with `args`, at most 20 s.
pub fn reload_with<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Output {
    let mut cmd = Command::new("sh");
    cmd.arg(HELPER).args(args);
    bounded(cmd)
}

/// Runs `cmd` to its end, at most 20 s.
pub fn bounded(mut cmd: Command) -> Output {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    finish(child)
}

/// Waits for `child` (its output piped) to end, at most 20 s.
pub fn finish(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("pid {} did not finish within 20 s", child.id());
        }
        thread::sleep(Duration::from_millis(5));
    }
    child.wait_with_output().unwrap()
}

/// Waits until the pid file names a pid other than `old` (the helper's
/// handover), at most 15 s; returns it.
pub fn await_new_pid(pidfile: &Path, old: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let pid = fs::read_to_string(pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok());
        if let Some(pid) = pid
            && pid != old
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "the pid file still names {old}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Field 22 of `/proc/<pid>/stat`, the start time (counted after the
/// last `)`, since the command name may contain spaces and parentheses).
pub fn start_time(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(") ")?;
    rest.split(' ').nth(19).map(str::to_owned)
}

/// Whether `pid` is running (a zombie is not).
pub fn alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with('Z')),
        Err(_) => false,
    }
}

pub fn wait_gone(pid: u32, limit: Duration) {
    let deadline = Instant::now() + limit;
    while alive(pid) {
        assert!(
            Instant::now() < deadline,
            "pid {pid} still runs after {limit:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether `pid` has a SIGHUP pending (`ShdPnd` in its status): sent to
/// it while it is stopped, not yet handled.
pub fn hup_pending(pid: u32) -> bool {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("ShdPnd:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & 1 != 0)
}
