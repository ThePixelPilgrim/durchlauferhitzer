//! The bench's own descriptor limit: `static-small-1k-idle` holds about
//! 1 010 sockets, beyond a common soft `RLIMIT_NOFILE` of 1024. Before
//! such a scenario the soft limit is raised to the hard one with
//! util-linux's `prlimit` on this process (std only: no `setrlimit`
//! binding); the server inherits it and checks its own budget
//! (NFR-RES-05). If it stays too low the scenario fails at startup.

use std::fs;
use std::process::Command;

/// Descriptors kept free beside the idle connections: the load's
/// clients, the backend's, the strace pass's pipes, stdio.
pub const MARGIN: u64 = 64;

/// The soft and hard `Max open files` of a `/proc/<pid>/limits` text
/// (`unlimited` as `u64::MAX`).
pub fn parse_nofile(limits: &str) -> Option<(u64, u64)> {
    let rest = limits
        .lines()
        .find_map(|l| l.strip_prefix("Max open files"))?;
    let mut fields = rest.split_whitespace().map(|v| match v {
        "unlimited" => Some(u64::MAX),
        n => n.parse().ok(),
    });
    Some((fields.next()??, fields.next()??))
}

fn nofile() -> Result<(u64, u64), String> {
    let text =
        fs::read_to_string("/proc/self/limits").map_err(|e| format!("/proc/self/limits: {e}"))?;
    parse_nofile(&text).ok_or_else(|| "no Max open files in /proc/self/limits".to_string())
}

/// Makes sure the soft limit is at least `needed`, raising it to the
/// hard limit when below; the error says how far it got.
pub fn ensure(needed: u64) -> Result<(), String> {
    let (soft, hard) = nofile()?;
    if soft >= needed {
        return Ok(());
    }
    let value = if hard == u64::MAX {
        "unlimited".to_string()
    } else {
        hard.to_string()
    };
    let raised = Command::new("prlimit")
        .args(["--pid", &std::process::id().to_string()])
        .arg(format!("--nofile={value}:{value}"))
        .status()
        .map_err(|e| format!("prlimit: {e}"))?;
    let (soft, hard) = nofile()?;
    if !raised.success() || soft < needed {
        return Err(format!(
            "open files: soft limit {soft} (hard {hard}), {needed} needed; raise `ulimit -n`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: &str = "Limit                     Soft Limit           Hard Limit           Units     \n\
        Max processes             63445                63445                processes \n\
        Max open files            1024                 524288               files     \n";

    #[test]
    fn the_open_files_line() {
        assert_eq!(parse_nofile(LIMITS), Some((1024, 524288)));
        assert_eq!(
            parse_nofile(
                "Max open files            unlimited            unlimited            files\n"
            ),
            Some((u64::MAX, u64::MAX))
        );
        assert_eq!(parse_nofile("Max processes 1 2 processes\n"), None);
        assert_eq!(parse_nofile("Max open files x 2 files\n"), None);
    }

    #[test]
    fn the_current_limit_is_enough_for_itself() {
        let (soft, _) = nofile().unwrap();
        assert_eq!(ensure(soft), Ok(()));
    }
}
