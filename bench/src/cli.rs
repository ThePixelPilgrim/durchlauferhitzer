//! The command line:
//! `bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b]
//! [--syscalls-only] [--netem <spec> [--pause S]] [--commit <sha>]`.

use std::path::PathBuf;
use std::time::Duration;

use crate::scenario::Scenario;

/// The longest `--duration`, in seconds.
pub const MAX_DURATION_SECS: f64 = 3600.0;

pub const USAGE: &str = "usage: bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b] [--syscalls-only] [--netem <spec> [--pause S]] [--commit <sha>]";

#[derive(Debug, PartialEq)]
pub struct Args {
    pub binary: PathBuf,
    pub out: PathBuf,
    pub runs: u32,
    pub duration: Duration,
    pub scenarios: Vec<Scenario>,
    /// Only the strace pass (no measured window): the row has no CPU
    /// or RSS numbers.
    pub syscalls_only: bool,
    /// `tc netem` arguments: the server behind a veth pair (netem.rs).
    pub netem: Option<String>,
    /// A pause (every packet dropped) inside each measured window.
    pub pause: Option<Duration>,
    /// The commit `--binary` was built from (the rows' label).
    pub commit: Option<String>,
}

/// A commit: 7 to 40 lowercase hex digits.
fn commit_sha(value: &str) -> Result<String, String> {
    let ok = (7..=40).contains(&value.len())
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if ok {
        Ok(value.to_string())
    } else {
        Err(format!(
            "--commit {value:?}: the measured binary's commit, 7 to 40 lowercase hex digits"
        ))
    }
}

/// The rows' commit label: it names the measured binary, so it comes
/// from `--commit` (checked), else it is `local`; never from the checkout
/// the bench runs in, which need not be the binary's.
pub fn commit_label(flag: Option<&str>) -> String {
    flag.unwrap_or("local").to_string()
}

/// The rows' runner label: `BENCH_RUNNER` (`env`), else `local`. A
/// plain label of 1 to 64 letters, digits, `.`, `_` and `-` (what
/// `scripts/bench-chart.py` charts); anything else is refused, so it
/// can never add a CSV column.
pub fn runner_label(env: Option<&str>) -> Result<String, String> {
    let Some(value) = env else {
        return Ok("local".to_string());
    };
    let ok = (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if ok {
        Ok(value.to_string())
    } else {
        Err(format!(
            "BENCH_RUNNER {value:?}: 1 to 64 letters, digits, '.', '_' or '-'"
        ))
    }
}

/// A netem spec: words of letters, digits, `.` and `%` (`delay 50ms`,
/// `loss 1%`), at least one.
fn netem_spec(value: &str) -> Result<String, String> {
    let ok = value.split_whitespace().next().is_some()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '%' | ' '));
    if ok {
        Ok(value.to_string())
    } else {
        Err(format!(
            "--netem {value:?}: netem arguments, e.g. \"delay 50ms\""
        ))
    }
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut binary = None;
    let mut out = None;
    let mut runs = 5;
    let mut duration = Duration::from_secs(10);
    let mut scenarios = Scenario::DEFAULT.to_vec();
    let mut syscalls_only = false;
    let mut netem = None;
    let mut pause = None;
    let mut commit = None;
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        if flag == "--syscalls-only" {
            syscalls_only = true;
            continue;
        }
        let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--binary" => binary = Some(PathBuf::from(value)),
            "--out" => out = Some(PathBuf::from(value)),
            "--runs" => {
                runs = value
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("--runs {value}: a positive number"))?;
            }
            "--duration" => {
                let secs: f64 = value
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s > 0.0 && *s <= MAX_DURATION_SECS)
                    .ok_or_else(|| format!("--duration {value}: seconds, above 0, at most 3600"))?;
                duration = Duration::from_secs_f64(secs);
            }
            "--scenarios" => {
                scenarios = value
                    .split(',')
                    .map(|name| {
                        Scenario::from_name(name).ok_or_else(|| format!("unknown scenario {name}"))
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--netem" => netem = Some(netem_spec(&value)?),
            "--pause" => {
                let secs: f64 = value
                    .parse()
                    .ok()
                    .filter(|s: &f64| s.is_finite() && *s > 0.0 && *s <= 60.0)
                    .ok_or_else(|| format!("--pause {value}: seconds, above 0, at most 60"))?;
                pause = Some(Duration::from_secs_f64(secs));
            }
            "--commit" => commit = Some(commit_sha(&value)?),
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    if pause.is_some() && netem.is_none() {
        return Err("--pause needs --netem".to_string());
    }
    if pause.is_some_and(|p| p * 3 > duration) {
        return Err("--pause: at most a third of --duration".to_string());
    }
    Ok(Args {
        binary: binary.ok_or("--binary is required")?,
        out: out.ok_or("--out is required")?,
        runs,
        duration,
        scenarios,
        syscalls_only,
        netem,
        pause,
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(text: &str) -> Result<Args, String> {
        parse(text.split_whitespace().map(String::from))
    }

    #[test]
    fn defaults() {
        let a = args("--binary b --out o.csv").unwrap();
        assert_eq!(a.runs, 5);
        assert_eq!(a.duration, Duration::from_secs(10));
        assert_eq!(a.scenarios, Scenario::DEFAULT.to_vec());
        assert!(!a.syscalls_only);
        assert_eq!((a.netem, a.pause), (None, None));
    }

    #[test]
    fn netem_and_pause() {
        let a = parse(
            [
                "--binary",
                "b",
                "--out",
                "o",
                "--netem",
                "delay 50ms",
                "--pause",
                "2",
            ]
            .map(String::from),
        )
        .unwrap();
        assert_eq!(a.netem.as_deref(), Some("delay 50ms"));
        assert_eq!(a.pause, Some(Duration::from_secs(2)));
        let a =
            parse(["--binary", "b", "--out", "o", "--netem", "loss 1%"].map(String::from)).unwrap();
        assert_eq!(a.netem.as_deref(), Some("loss 1%"));
        for bad in [
            &["--netem", "delay 1ms; reboot"][..],
            &["--netem", " "],
            &["--pause", "2"],
            &["--netem", "loss 1%", "--pause", "0"],
            &["--netem", "loss 1%", "--pause", "4"],
        ] {
            let mut v = vec!["--binary", "b", "--out", "o"];
            v.extend_from_slice(bad);
            assert!(parse(v.into_iter().map(String::from)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_commit_flag() {
        let a = args("--binary b --out o --commit 999724c").unwrap();
        assert_eq!(a.commit.as_deref(), Some("999724c"));
        assert_eq!(args("--binary b --out o").unwrap().commit, None);
        let full = "999724c1d9f8eee3e4a147846d1536020cf0b997";
        let a = args(&format!("--binary b --out o --commit {full}")).unwrap();
        assert_eq!(a.commit.as_deref(), Some(full));
        for bad in ["99972", "999724C", "999724g", "local", &format!("{full}0")] {
            assert!(
                args(&format!("--binary b --out o --commit {bad}")).is_err(),
                "{bad}"
            );
        }
    }

    /// The label names the measured binary: the flag, else
    /// `local` (never the bench's own checkout, never an unchecked
    /// environment variable).
    #[test]
    fn the_commit_label() {
        assert_eq!(commit_label(Some("abc1234")), "abc1234");
        assert_eq!(commit_label(None), "local");
    }

    /// `BENCH_RUNNER`: a plain label (letters, digits, `.`, `_`, `-`; 1
    /// to 64), so it can never add a CSV column or markup; unset is
    /// `local`, anything else is refused.
    #[test]
    fn the_runner_label() {
        assert_eq!(runner_label(None).unwrap(), "local");
        assert_eq!(
            runner_label(Some("gha-ubuntu-24.04-x86_64-netem-loss1pct")).unwrap(),
            "gha-ubuntu-24.04-x86_64-netem-loss1pct"
        );
        let long = "x".repeat(65);
        for bad in ["", "a,b", "a b", "gha\n", "<img>", "[x](y)", "ü", &long] {
            assert!(runner_label(Some(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(runner_label(Some(&long[..64])).unwrap().len(), 64);
    }

    #[test]
    fn syscalls_only_takes_no_value() {
        let a = args("--syscalls-only --binary b --out o.csv").unwrap();
        assert!(a.syscalls_only);
        let a = args("--binary b --out o.csv --syscalls-only").unwrap();
        assert!(a.syscalls_only);
    }

    #[test]
    fn every_option() {
        let a = args(
            "--binary b --out o.csv --runs 2 --duration 0.5 --scenarios proxy-fresh,static-small,static-small-1k-idle",
        )
        .unwrap();
        assert_eq!(a.runs, 2);
        assert_eq!(a.duration, Duration::from_millis(500));
        assert_eq!(
            a.scenarios,
            vec![
                Scenario::ProxyFresh,
                Scenario::StaticSmall,
                Scenario::StaticSmall1kIdle
            ]
        );
    }

    #[test]
    fn errors() {
        assert!(args("--out o.csv").is_err());
        assert!(args("--binary b --out o --runs 0").is_err());
        assert!(args("--binary b --out o --duration -1").is_err());
        assert!(args("--binary b --out o --scenarios static").is_err());
        assert!(args("--binary b --out o --what 1").is_err());
        assert!(args("--binary b --out").is_err());
    }
}
