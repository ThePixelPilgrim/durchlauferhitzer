//! The command line:
//! `bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b]
//! [--syscalls-only]`.

use std::path::PathBuf;
use std::time::Duration;

use crate::scenario::Scenario;

pub const USAGE: &str = "usage: bench --binary <path> --out <csv> [--runs N] [--duration S] [--scenarios a,b] [--syscalls-only]";

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
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut binary = None;
    let mut out = None;
    let mut runs = 5;
    let mut duration = Duration::from_secs(10);
    let mut scenarios = Scenario::DEFAULT.to_vec();
    let mut syscalls_only = false;
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
                    .filter(|s: &f64| s.is_finite() && *s > 0.0 && *s <= 3600.0)
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
            _ => return Err(format!("unknown argument {flag}")),
        }
    }
    Ok(Args {
        binary: binary.ok_or("--binary is required")?,
        out: out.ok_or("--out is required")?,
        runs,
        duration,
        scenarios,
        syscalls_only,
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
