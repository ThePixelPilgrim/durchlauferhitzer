//! The system-call pass: `strace -c -f -p <server>` (through `sudo -n`
//! unless the bench runs as root) attached for a fixed window, the same
//! load driven inside it, and the summary's total calls. This pass is
//! slow (every call stops the server) and never feeds the CPU numbers.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::load::{self, Tally, Target};
use crate::measure;
use crate::server::{self, Server};

/// How long strace may take to attach to every thread.
const ATTACH: Duration = Duration::from_secs(3);
/// Time after the load for the last closes, before strace stops.
const SETTLE: Duration = Duration::from_secs(3);

pub struct Traced {
    /// The calls of every server thread over the window.
    pub calls: u64,
    pub tally: Tally,
}

pub fn pass(
    server: &Server,
    target: &Target,
    concurrency: usize,
    duration: Duration,
) -> Result<Traced, String> {
    let window = ATTACH + duration + SETTLE;
    let mut cmd = if server::is_root() {
        Command::new("timeout")
    } else {
        let mut c = Command::new("sudo");
        c.args(["-n", "timeout"]);
        c
    };
    cmd.args(["-s", "INT", &format!("{:.1}", window.as_secs_f64())])
        .args(["strace", "-c", "-f", "-p", &server.pid().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().map_err(|e| format!("strace: {e}"))?;
    // The summary goes to stderr when strace stops: read it meanwhile.
    let mut stderr = child.stderr.take().ok_or("strace: no stderr")?;
    let reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    while !server.all_traced() {
        if start.elapsed() > ATTACH || child.try_wait().ok().flatten().is_some() {
            let _ = child.wait();
            let text = reader.join().unwrap_or_default();
            return Err(format!("strace never attached: {}", text.trim()));
        }
        thread::sleep(Duration::from_millis(20));
    }
    let tally = load::drive(target, concurrency, duration);
    let late = start.elapsed() + Duration::from_millis(500) > window;
    let _ = child.wait();
    let text = reader.join().unwrap_or_default();
    if late {
        return Err("the load outlasted the strace window".to_string());
    }
    let calls = measure::strace_total_calls(&text)
        .ok_or_else(|| format!("no strace summary: {}", text.trim()))?;
    Ok(Traced { calls, tally })
}
