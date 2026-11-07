//! `--netem <spec>` (NFR-PERF-05): the server in its own network
//! namespace, joined to the bench's by a veth pair whose two ends each
//! carry `tc qdisc ... netem <spec>` (so a delay applies in each
//! direction), the clients and the stub backend in the bench's own
//! namespace. Root only; needs `ip` and `tc` (iproute2) and a kernel with
//! `sch_netem`. `--pause <s>` drops every packet on both ends (`netem
//! loss 100%`) for that long, a third into the measured window, and the
//! window's completions give the recovery after it.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Command;
use std::time::{Duration, Instant};

/// The server's namespace.
pub const NETNS: &str = "dlh-bench-srv";
/// The veth ends: ours, and the server's.
const OURS: &str = "dlh-bench-c";
const THEIRS: &str = "dlh-bench-s";
/// Our address (the clients and the backend), and the server's.
pub const CLIENT_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 77, 0, 1));
pub const SERVER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(10, 77, 0, 2));
/// The server's port inside its namespace (nothing else listens there).
pub const SERVER_PORT: u16 = 8080;

/// The namespace and the veth pair, removed on drop.
pub struct Net {
    spec: Vec<String>,
}

/// Runs one command; its failure, with its stderr, is the error.
fn run(args: &[&str]) -> Result<(), String> {
    let (cmd, rest) = args.split_first().ok_or("an empty command")?;
    let out = Command::new(cmd)
        .args(rest)
        .output()
        .map_err(|e| format!("{}: {e}", args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{}: {} {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The `tc` commands that set `netem <args>` on both ends (`verb`:
/// `add` or `change`).
pub fn qdisc_commands(verb: &str, args: &[String]) -> Vec<Vec<String>> {
    let tail = |dev: &str| {
        let mut v: Vec<String> = ["qdisc", verb, "dev", dev, "root", "netem"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        v.extend(args.iter().cloned());
        v
    };
    let mut ours = vec!["tc".to_string()];
    ours.extend(tail(OURS));
    let mut theirs = vec!["tc".to_string(), "-n".to_string(), NETNS.to_string()];
    theirs.extend(tail(THEIRS));
    vec![ours, theirs]
}

/// The `netem` arguments that drop every packet.
pub fn pause_args() -> Vec<String> {
    vec!["loss".to_string(), "100%".to_string()]
}

impl Net {
    /// Sets the namespace, the pair and `netem <spec>` up (a leftover of
    /// an earlier run is removed first).
    pub fn up(spec: &str) -> Result<Net, String> {
        let _ = run(&["ip", "netns", "del", NETNS]);
        let _ = run(&["ip", "link", "del", OURS]);
        let net = Net {
            spec: spec.split_whitespace().map(String::from).collect(),
        };
        let ours = format!("{CLIENT_IP}/24");
        let theirs = format!("{SERVER_IP}/24");
        for cmd in [
            vec!["ip", "netns", "add", NETNS],
            vec![
                "ip", "link", "add", OURS, "type", "veth", "peer", "name", THEIRS,
            ],
            vec!["ip", "link", "set", THEIRS, "netns", NETNS],
            vec!["ip", "addr", "add", &ours, "dev", OURS],
            vec!["ip", "link", "set", OURS, "up"],
            vec!["ip", "-n", NETNS, "addr", "add", &theirs, "dev", THEIRS],
            vec!["ip", "-n", NETNS, "link", "set", THEIRS, "up"],
            vec!["ip", "-n", NETNS, "link", "set", "lo", "up"],
        ] {
            run(&cmd)?;
        }
        net.set("add", &net.spec)?;
        Ok(net)
    }

    fn set(&self, verb: &str, args: &[String]) -> Result<(), String> {
        for cmd in qdisc_commands(verb, args) {
            let cmd: Vec<&str> = cmd.iter().map(String::as_str).collect();
            run(&cmd)?;
        }
        Ok(())
    }

    /// Drops every packet for `pause`, then restores the spec.
    pub fn pause(&self, pause: Duration) -> Result<(), String> {
        self.set("change", &pause_args())?;
        std::thread::sleep(pause);
        self.set("change", &self.spec)
    }

    /// The server's listen address.
    pub fn server_addr() -> SocketAddr {
        SocketAddr::new(SERVER_IP, SERVER_PORT)
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        // Deleting the namespace deletes its veth end, and the pair with it.
        let _ = run(&["ip", "netns", "del", NETNS]);
    }
}

/// What the completions of a window with a pause show.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recovery {
    /// Completions per second before the pause.
    pub rate_before: f64,
    /// From the pause's end to the first [`BUCKET`] with at least half
    /// the rate before; `None` if none came.
    pub recovery: Option<Duration>,
    /// The longest time without a completion.
    pub gap: Duration,
}

/// The bucket the recovery is measured in.
pub const BUCKET: Duration = Duration::from_millis(250);

/// The recovery in a window that started at `start` and ended at `end`,
/// with a pause from `paused` to `resumed`, from the completion times
/// `done`.
pub fn recovery(
    start: Instant,
    end: Instant,
    paused: Instant,
    resumed: Instant,
    done: &[Instant],
) -> Recovery {
    let mut done = done.to_vec();
    done.sort();
    let before = done.iter().filter(|&&t| t < paused).count();
    let secs = paused.saturating_duration_since(start).as_secs_f64();
    let rate_before = if secs > 0.0 {
        before as f64 / secs
    } else {
        0.0
    };
    let want = (rate_before * BUCKET.as_secs_f64() / 2.0).max(1.0);
    let mut recovery = None;
    let mut at = resumed;
    while at + BUCKET <= end {
        let n = done.iter().filter(|&&t| t >= at && t < at + BUCKET).count();
        if n as f64 >= want {
            recovery = Some(at.saturating_duration_since(resumed));
            break;
        }
        at += BUCKET / 5;
    }
    let mut gap = Duration::ZERO;
    let mut last = start;
    for &t in done.iter().chain(std::iter::once(&end)) {
        gap = gap.max(t.saturating_duration_since(last));
        last = last.max(t);
    }
    Recovery {
        rate_before,
        recovery,
        gap,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_qdisc_commands_cover_both_ends() {
        let spec: Vec<String> = ["delay", "50ms"].iter().map(|s| s.to_string()).collect();
        let cmds = qdisc_commands("add", &spec);
        assert_eq!(
            cmds[0].join(" "),
            "tc qdisc add dev dlh-bench-c root netem delay 50ms"
        );
        assert_eq!(
            cmds[1].join(" "),
            "tc -n dlh-bench-srv qdisc add dev dlh-bench-s root netem delay 50ms"
        );
        assert_eq!(
            qdisc_commands("change", &pause_args())[1].join(" "),
            "tc -n dlh-bench-srv qdisc change dev dlh-bench-s root netem loss 100%"
        );
    }

    #[test]
    fn the_recovery_is_the_first_bucket_at_half_the_rate() {
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        // 100/s for 3 s, nothing for the 2 s pause and 400 ms after it,
        // 20/s for 600 ms, then 100/s again until 9 s.
        let mut done: Vec<Instant> = (0..300).map(|i| ms(i * 10)).collect();
        done.extend((0..12).map(|i| ms(5400 + i * 50)));
        done.extend((0..300).map(|i| ms(6000 + i * 10)));
        let r = recovery(start, ms(9000), ms(3000), ms(5000), &done);
        assert!((r.rate_before - 100.0).abs() < 1e-9, "{r:?}");
        // Half of 100/s in 250 ms is 12.5 completions: from 5.85 s on.
        assert_eq!(r.recovery, Some(Duration::from_millis(850)));
        assert_eq!(r.gap, Duration::from_millis(2410));
    }

    #[test]
    fn no_recovery_when_nothing_comes_back() {
        let start = Instant::now();
        let ms = |n: u64| start + Duration::from_millis(n);
        let done: Vec<Instant> = (0..100).map(|i| ms(i * 10)).collect();
        let r = recovery(start, ms(3000), ms(1000), ms(1500), &done);
        assert_eq!(r.recovery, None);
        assert_eq!(r.gap, Duration::from_millis(2010));
    }
}
