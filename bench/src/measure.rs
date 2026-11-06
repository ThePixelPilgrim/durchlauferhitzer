//! The numbers of one measured window: the server's CPU time from
//! `/proc/<pid>/stat` (utime + stime, all its threads), its resident
//! memory from `/proc/<pid>/status`, the total of an `strace -c`
//! summary, and the per-request arithmetic.

/// Clock ticks per second of `/proc/<pid>/stat` (`USER_HZ`, 100 on every
/// Linux architecture this project builds for).
pub const CLK_TCK: u64 = 100;

/// utime + stime, in clock ticks, of a `/proc/<pid>/stat` text.
pub fn cpu_ticks(stat: &str) -> Option<u64> {
    // The fields after the comm, which may hold spaces and `)`: field 3
    // (state) is the first, so utime (14) and stime (15) are the 12th and
    // 13th.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_ascii_whitespace().skip(11);
    let utime: u64 = fields.next()?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime + stime)
}

fn status_field<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    status.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key == name).then(|| value.trim())
    })
}

/// `VmRSS` in KiB of a `/proc/<pid>/status` text.
pub fn rss_kib(status: &str) -> Option<u64> {
    status_field(status, "VmRSS")?
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()
}

/// `TracerPid` of a `/proc/<pid>/status` text.
pub fn tracer_pid(status: &str) -> Option<u32> {
    status_field(status, "TracerPid")?.parse().ok()
}

/// CPU microseconds per request for `ticks` clock ticks over `requests`
/// complete requests; None without a request.
pub fn cpu_us_per_req(ticks: u64, requests: u64) -> Option<f64> {
    let us = ticks as f64 * (1_000_000 / CLK_TCK) as f64;
    (requests > 0).then(|| us / requests as f64)
}

/// `total` per request; None without a request.
pub fn per_request(total: u64, requests: u64) -> Option<f64> {
    (requests > 0).then(|| total as f64 / requests as f64)
}

/// The total number of calls in an `strace -c` summary (its `total` line).
pub fn strace_total_calls(summary: &str) -> Option<u64> {
    // `% time, seconds, usecs/call, calls, [errors,] total`.
    let line = summary
        .lines()
        .rev()
        .find(|l| l.split_ascii_whitespace().last() == Some("total"))?;
    line.split_ascii_whitespace().nth(3)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A server whose comm has a space and a parenthesis: the fields count
    // from the last `)`. utime (field 14) 1234, stime (field 15) 566.
    const STAT: &str = "4242 (durch (x) y) S 1 4242 4242 0 -1 4194560 300 0 0 0 1234 566 0 0 20 0 6 0 99 12345678 900 18446744073709551615 1 1 0 0 0 0 0 4096 0 0 0 0 17 1 0 0 0 0 0";

    #[test]
    fn cpu_ticks_are_utime_plus_stime() {
        assert_eq!(cpu_ticks(STAT), Some(1800));
        assert_eq!(cpu_ticks("4242 (x) S 1 2"), None);
        assert_eq!(cpu_ticks(""), None);
    }

    #[test]
    fn cpu_per_request() {
        // 150 ticks = 1.5 s = 1 500 000 us over 1000 requests.
        assert_eq!(cpu_us_per_req(150, 1000), Some(1500.0));
        assert_eq!(cpu_us_per_req(1, 4), Some(2500.0));
        assert_eq!(cpu_us_per_req(0, 10), Some(0.0));
        assert_eq!(cpu_us_per_req(150, 0), None);
    }

    #[test]
    fn calls_per_request() {
        assert_eq!(per_request(12_345, 1000), Some(12.345));
        assert_eq!(per_request(5, 0), None);
    }

    #[test]
    fn status_fields() {
        let status = "Name:\tdurchlauferhitzer\nState:\tS (sleeping)\nTracerPid:\t77\nVmRSS:\t    3172 kB\nThreads:\t5\n";
        assert_eq!(rss_kib(status), Some(3172));
        assert_eq!(tracer_pid(status), Some(77));
        assert_eq!(rss_kib("Name:\tx\n"), None);
    }

    #[test]
    fn strace_summary_total() {
        let with_errors = "\
% time     seconds  usecs/call     calls    errors syscall
------ ----------- ----------- --------- --------- ----------------
 41.20    0.012000           3      4000           sendto
 30.00    0.009000           2      3500       500 recvfrom
------ ----------- ----------- --------- --------- ----------------
100.00    0.029000           3      7500       500 total
";
        assert_eq!(strace_total_calls(with_errors), Some(7500));
        let without = "100.00    0.001000           1       250           total\n";
        assert_eq!(strace_total_calls(without), Some(250));
        assert_eq!(
            strace_total_calls(
                "strace: attach: ptrace(PTRACE_SEIZE, 1): Operation not permitted\n"
            ),
            None
        );
    }
}
