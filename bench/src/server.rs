//! The server under test: a temp dir with the two files and the config
//! (one static root, one proxy route to the stub backend, default limits
//! but three for the idle connections, default containment, `user nobody` when run as
//! root), the binary started
//! on a free loopback port, its stderr drained in a thread (the bench's
//! CPU, not the server's), and its `/proc` numbers.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::measure;
use crate::scenario::{HOST, LARGE, SMALL};

/// How long the server may take to listen.
const START_PATIENCE: Duration = Duration::from_secs(10);
/// The last stderr lines kept for an error message.
const KEEP_LINES: usize = 20;

/// An I/O error, prefixed with what it was about.
fn io(what: &'static str) -> impl Fn(std::io::Error) -> String {
    move |e| format!("{what}: {e}")
}

pub struct Server {
    child: Child,
    pub addr: SocketAddr,
    dir: PathBuf,
    last_lines: Arc<Mutex<VecDeque<String>>>,
}

/// Whether this process runs as root (effective uid 0).
pub fn is_root() -> bool {
    fs::read_to_string("/proc/self/status").is_ok_and(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().nth(1))
            == Some("0")
    })
}

fn free_addr() -> std::io::Result<SocketAddr> {
    TcpListener::bind("127.0.0.1:0")?.local_addr()
}

fn write_world_readable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::write(path, bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
}

/// The bench server's `keepalive_timeout` (see [`config`]).
pub const KEEPALIVE_MINUTES: u64 = 125;

/// The bench config: default limits but three, for
/// `static-small-1k-idle`'s 1 000 idle connections (one server serves
/// every scenario of a run, and these only matter there):
/// `max_connections 2048` and `max_connections_per_ip 2048` leave room
/// beside them (every client shares 127.0.0.1), and `keepalive_timeout
/// 125m` outlasts their hold at the longest `--duration` (3600 s: two
/// windows plus about 8 s). The other scenarios never hold more than a
/// few connections, and their clients close every connection after the
/// window, so no limit is reached and no keep-alive deadline expires
/// there either way.
pub fn config(addr: SocketAddr, root: &Path, backend: SocketAddr, as_root: bool) -> String {
    let user = if as_root { "user nobody;\n" } else { "" };
    format!(
        "listen {addr};\n{user}limits {{\n  max_connections 2048;\n  max_connections_per_ip 2048;\n  keepalive_timeout {KEEPALIVE_MINUTES}m;\n}}\nhost {HOST} {{\n  path / {{ static {root}; }}\n  path /proxy {{ proxy {backend}; strip_prefix; max_request_body 1MiB; max_response_body unlimited; }}\n}}\n",
        root = root.display()
    )
}

/// Where the server runs: on loopback, or behind the netem veth pair in
/// its own namespace (`--netem`, netem.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Loopback,
    Netns,
}

impl Server {
    pub fn start(
        binary: &Path,
        backend: SocketAddr,
        tag: &str,
        place: Place,
    ) -> Result<Server, String> {
        let dir = std::env::temp_dir().join(format!("dlh-bench-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("www");
        fs::create_dir_all(&root).map_err(io("temp dir"))?;
        for d in [&dir, &root] {
            fs::set_permissions(d, fs::Permissions::from_mode(0o755)).map_err(io("temp dir"))?;
        }
        write_world_readable(&root.join("small.bin"), &vec![b's'; SMALL as usize])
            .map_err(io("small.bin"))?;
        write_world_readable(&root.join("large.bin"), &vec![b'l'; LARGE as usize])
            .map_err(io("large.bin"))?;
        let addr = match place {
            Place::Loopback => free_addr().map_err(io("free port"))?,
            Place::Netns => crate::netem::Net::server_addr(),
        };
        let conf = dir.join("bench.conf");
        write_world_readable(&conf, config(addr, &root, backend, is_root()).as_bytes())
            .map_err(io("config"))?;
        // `ip netns exec` execs the binary (no fork): the child's pid is
        // the server's.
        let mut command = match place {
            Place::Loopback => Command::new(binary),
            Place::Netns => {
                let mut c = Command::new("ip");
                c.args(["netns", "exec", crate::netem::NETNS]).arg(binary);
                c
            }
        };
        let mut child = command
            .arg(&conf)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(io("start the server"))?;
        let last_lines = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = child.stderr.take() {
            let keep = Arc::clone(&last_lines);
            thread::spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { return };
                    if let Ok(mut q) = keep.lock() {
                        if q.len() == KEEP_LINES {
                            q.pop_front();
                        }
                        q.push_back(line);
                    }
                }
            });
        }
        let mut server = Server {
            child,
            addr,
            dir,
            last_lines,
        };
        let deadline = Instant::now() + START_PATIENCE;
        loop {
            if let Some(status) = server.exited() {
                return Err(format!(
                    "the server exited ({status}): {}",
                    server.last_lines()
                ));
            }
            if TcpStream::connect(addr).is_ok() {
                return Ok(server);
            }
            if Instant::now() > deadline {
                return Err("the server never listened".to_string());
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The exit status, once the server has exited.
    pub fn exited(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.to_string()),
            Ok(None) => None,
            Err(e) => Some(format!("wait: {e}")),
        }
    }

    pub fn last_lines(&self) -> String {
        self.last_lines
            .lock()
            .map(|q| q.iter().cloned().collect::<Vec<_>>().join(" | "))
            .unwrap_or_default()
    }

    fn proc_file(&self, name: &str) -> Result<String, String> {
        let path = format!("/proc/{}/{name}", self.pid());
        fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))
    }

    /// utime + stime of every thread so far, in clock ticks.
    pub fn cpu_ticks(&self) -> Result<u64, String> {
        measure::cpu_ticks(&self.proc_file("stat")?).ok_or_else(|| "unreadable stat".to_string())
    }

    pub fn rss_kib(&self) -> Option<u64> {
        measure::rss_kib(&self.proc_file("status").ok()?)
    }

    /// Whether every thread of the server is traced.
    pub fn all_traced(&self) -> bool {
        let Ok(tasks) = fs::read_dir(format!("/proc/{}/task", self.pid())) else {
            return false;
        };
        let mut any = false;
        for task in tasks.flatten() {
            let status = fs::read_to_string(task.path().join("status")).unwrap_or_default();
            if measure::tracer_pid(&status).unwrap_or(0) == 0 {
                return false;
            }
            any = true;
        }
        any
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let backend: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        let text = config(addr, Path::new("/tmp/x/www"), backend, true);
        assert_eq!(
            text,
            "listen 127.0.0.1:8080;\nuser nobody;\nlimits {\n  max_connections 2048;\n  max_connections_per_ip 2048;\n  keepalive_timeout 125m;\n}\nhost bench.test {\n  path / { static /tmp/x/www; }\n  path /proxy { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body unlimited; }\n}\n"
        );
        assert!(!config(addr, Path::new("/w"), backend, false).contains("user"));
    }

    /// The idle connections are held from before the warm-up to the end
    /// of the strace pass: the keep-alive timeout outlasts that at the
    /// longest `--duration`.
    #[test]
    fn idle_connections_outlive_the_longest_hold() {
        let window = Duration::from_secs_f64(crate::cli::MAX_DURATION_SECS);
        let hold = crate::WARM_UP
            + window
            + crate::SETTLE
            + crate::strace::ATTACH
            + window
            + crate::strace::SETTLE;
        assert!(
            Duration::from_secs(KEEPALIVE_MINUTES * 60) > hold,
            "{hold:?}"
        );
    }
}
