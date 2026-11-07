//! A running server for the tests that run the binary (`child.rs`
//! includes this file and re-exports it): [`Running`] kills it (SIGKILL)
//! when dropped and fails the test if seccomp had killed it; signals are
//! sent with the shell's `kill` ([`signal_pid`]); its stderr is read line
//! by line while it runs ([`Lines`]); [`get`] sends one request.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use super::{PATIENCE, Setup, command, text};

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
                // `free_port` leaves only another program binding the port.
                let race = if stderr.contains("Address already in use") {
                    " (another program bound the port from `free_port` before \
                     the server did)"
                } else {
                    ""
                };
                panic!("the server exited early ({status}){race}: {stderr}");
            }
            // The connect alone is not enough: a normal start's bind probe
            // (`Probe::First`, `sys/socket.rs`) listens for a moment
            // first, and a connect it takes is reset when it closes. The
            // signalfd is made in `run_loop`, after the real listeners.
            if past_the_bind(self.child.id()) && TcpStream::connect(addr).is_ok() {
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
        self.end();
        let mut stderr = Vec::new();
        if let Some(mut err) = self.child.stderr.take() {
            err.read_to_end(&mut stderr).unwrap();
        }
        text(&stderr)
    }
}

impl Running {
    /// Kills (SIGKILL) and reaps the server, then fails the test if it had
    /// already been ended by `SIGSYS` (the seccomp `kill` action: a call
    /// missing from the allow-list); a reaped status keeps that signal
    /// whatever came after. While the test already panics, only says so.
    fn end(&mut self) {
        use std::os::unix::process::ExitStatusExt;
        let _ = self.child.kill();
        let Ok(status) = self.child.wait() else {
            return;
        };
        if status.signal() != Some(libc::SIGSYS) {
            return;
        }
        let message = "the server was killed by SIGSYS: a system call missing from the \
                       seccomp allow-list (driver/src/containment/filter.rs)";
        if thread::panicking() {
            eprintln!("{message}");
        } else {
            panic!("{message}");
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.end();
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

/// Whether the server `pid` has its signalfd: startup has bound the real
/// listeners (A11; the bind probe's listener is closed by then).
pub fn past_the_bind(pid: u32) -> bool {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten().any(|fd| {
        std::fs::read_link(fd.path()).is_ok_and(|link| link.as_os_str() == "anon_inode:[signalfd]")
    })
}
