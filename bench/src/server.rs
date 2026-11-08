//! The server under test: a temp dir with the two files and the config
//! (one static root, one proxy route to the stub backend and, for
//! `proxy-file`, a second one serving the backend's files, default limits
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
use crate::scenario::{HOST, LARGE, Routes, SMALL};

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
///
/// With `files`, also the `/file` route of `proxy-file`: a proxy route
/// to the same backend whose `X-Accel-Redirect: /protected/<name>`
/// names a file below `files` (FR-PROXY-32, FR-CFG-26).
pub fn config(
    addr: SocketAddr,
    root: &Path,
    backend: SocketAddr,
    as_root: bool,
    files: Option<&Path>,
) -> String {
    let user = if as_root { "user nobody;\n" } else { "" };
    let file_route = files.map_or_else(String::new, |files| {
        format!(
            "  path /file {{ proxy {backend}; strip_prefix; max_request_body 1MiB; max_response_body unlimited; internal_files x-accel-redirect /protected/ {}; }}\n",
            files.display()
        )
    });
    format!(
        "listen {addr};\n{user}limits {{\n  max_connections 2048;\n  max_connections_per_ip 2048;\n  keepalive_timeout {KEEPALIVE_MINUTES}m;\n}}\nhost {HOST} {{\n  path / {{ static {root}; }}\n  path /proxy {{ proxy {backend}; strip_prefix; max_request_body 1MiB; max_response_body unlimited; }}\n{file_route}}}\n",
        root = root.display()
    )
}

/// Whether the server whose `/proc/<pid>/fd` is `fds` has its signalfd:
/// startup has bound the real listeners, and its bind probe's listener
/// is closed by then.
pub fn past_the_bind(fds: &Path) -> bool {
    let Ok(entries) = fs::read_dir(fds) else {
        return false;
    };
    entries.flatten().any(|fd| {
        fs::read_link(fd.path()).is_ok_and(|link| link.as_os_str() == "anon_inode:[signalfd]")
    })
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
        routes: Routes,
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
        let files = match routes {
            Routes::Plain => None,
            Routes::WithFiles => {
                let files = dir.join("files");
                fs::create_dir_all(&files).map_err(io("temp dir"))?;
                fs::set_permissions(&files, fs::Permissions::from_mode(0o755))
                    .map_err(io("temp dir"))?;
                write_world_readable(&files.join("1k.bin"), &vec![b'f'; SMALL as usize])
                    .map_err(io("1k.bin"))?;
                Some(files)
            }
        };
        let addr = match place {
            Place::Loopback => free_addr().map_err(io("free port"))?,
            Place::Netns => crate::netem::Net::server_addr(),
        };
        let conf = dir.join("bench.conf");
        let text = config(addr, &root, backend, is_root(), files.as_deref());
        write_world_readable(&conf, text.as_bytes()).map_err(io("config"))?;
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
            // The connect alone is not enough: a normal start's bind
            // probe listens for a moment first, and a connection it takes
            // is reset when it closes. The signalfd is made after the
            // real listeners are bound.
            let fds = PathBuf::from(format!("/proc/{}/fd", server.pid()));
            if past_the_bind(&fds) && TcpStream::connect(addr).is_ok() {
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
        let text = config(addr, Path::new("/tmp/x/www"), backend, true, None);
        assert_eq!(
            text,
            "listen 127.0.0.1:8080;\nuser nobody;\nlimits {\n  max_connections 2048;\n  max_connections_per_ip 2048;\n  keepalive_timeout 125m;\n}\nhost bench.test {\n  path / { static /tmp/x/www; }\n  path /proxy { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; max_response_body unlimited; }\n}\n"
        );
        assert!(!config(addr, Path::new("/w"), backend, false, None).contains("user"));
    }

    /// Only `proxy-file`'s server has `internal_files`: every other
    /// scenario runs the config of before stage 8 (`the_config`).
    #[test]
    fn only_proxy_file_gets_internal_files() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let backend: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        let plain = config(addr, Path::new("/w"), backend, false, None);
        for s in crate::scenario::Scenario::ALL {
            let files = match s.routes() {
                Routes::Plain => None,
                Routes::WithFiles => Some(Path::new("/f")),
            };
            let text = config(addr, Path::new("/w"), backend, false, files);
            let proxy_file = s == crate::scenario::Scenario::ProxyFile;
            assert_eq!(text.contains("internal_files"), proxy_file, "{}", s.name());
            assert_eq!(text == plain, !proxy_file, "{}", s.name());
        }
    }

    /// `proxy-file`'s route (FR-PROXY-32, FR-CFG-26): the backend's
    /// `X-Accel-Redirect: /protected/1k.bin` names `1k.bin` below the
    /// files root.
    #[test]
    fn the_config_with_the_file_route() {
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let backend: SocketAddr = "127.0.0.1:3000".parse().unwrap();
        let text = config(
            addr,
            Path::new("/tmp/x/www"),
            backend,
            false,
            Some(Path::new("/tmp/x/files")),
        );
        assert!(
            text.ends_with(
                "  path /file { proxy 127.0.0.1:3000; strip_prefix; max_request_body 1MiB; \
                 max_response_body unlimited; internal_files x-accel-redirect /protected/ \
                 /tmp/x/files; }\n}\n"
            ),
            "{text}"
        );
    }

    /// A `/proc/<pid>/fd`-shaped directory: past the bind once one link
    /// names the signalfd; missing or without it, not yet.
    #[test]
    fn past_the_bind_needs_the_signalfd() {
        let dir = std::env::temp_dir().join(format!("dlh-bench-fdtest-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert!(!past_the_bind(&dir), "no directory");
        fs::create_dir_all(&dir).unwrap();
        assert!(!past_the_bind(&dir), "no descriptors");
        std::os::unix::fs::symlink("socket:[1234]", dir.join("3")).unwrap();
        std::os::unix::fs::symlink("anon_inode:[eventpoll]", dir.join("4")).unwrap();
        assert!(
            !past_the_bind(&dir),
            "a listener and epoll only: the bind probe"
        );
        std::os::unix::fs::symlink("anon_inode:[signalfd]", dir.join("5")).unwrap();
        assert!(past_the_bind(&dir));
        fs::remove_dir_all(&dir).unwrap();
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
