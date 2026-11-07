//! The command line:
//! `bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b]
//! [--syscalls-only] [--netem <spec> [--pause S]]`.

use std::path::PathBuf;
use std::time::Duration;

use crate::scenario::Scenario;

/// The longest `--duration`, in seconds.
pub const MAX_DURATION_SECS: f64 = 3600.0;

pub const USAGE: &str = "usage: bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b] [--syscalls-only] [--netem <spec> [--pause S]]";

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
