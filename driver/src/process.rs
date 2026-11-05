//! The process: the command line (FR-PROC-01), the startup sequence
//! (FR-PROC-04, A11), readiness (`--ready-fd`, FR-PROC-07), the signals
//! (A9) and the exit codes (FR-PROC-05a).
//!
//! Everything this module prints (the usage, `--check`'s answer, every
//! startup error) is printed before the stderr log sink exists, and
//! nothing is printed through std while it lives (FR-LOG-06): the sink
//! makes fd 2's file description non-blocking, which stdout may share.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use durchlauferhitzer_core::Core;
use durchlauferhitzer_core::config::{Config, Containment, load};

use crate::containment::{self, Files};
use crate::event_loop::{Driver, Exit};
use crate::log::{DriverLog, LogEntry, LogSink};
use crate::sys::file::{Openat2, open_dir_path, openat2_supported};
use crate::sys::signal::{Signal, SignalFd};
use crate::sys::socket::{Probe, tcp_listen};
use crate::systemd::{Activation, take_sockets};

mod budget;
mod privileges;
mod ready;

use ready::{Readiness, ReadyFd};

/// The usage text, printed to stderr for bad arguments (exit 1).
const USAGE: &str = "usage: durchlauferhitzer <config-path>\n       \
                     durchlauferhitzer --check <config-path>\n       \
                     durchlauferhitzer --ready-fd <n> <config-path>";

/// The listen backlog: the kernel's `SOMAXCONN` since Linux 5.4 (it caps
/// the backlog at `net.core.somaxconn`). Not `libc::SOMAXCONN`: that is
/// 128 on musl, which would reset a restart's queued connections beyond
/// it (FR-PROC-07).
const BACKLOG: u32 = 4096;

/// The exit codes (FR-PROC-05a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitStatus {
    /// Success, `--check` on a valid config included.
    Ok = 0,
    /// An invalid config, or bad arguments (with the usage).
    Config = 1,
    /// A startup failure (a missing root, a port in use, ...).
    Startup = 2,
}

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    /// `durchlauferhitzer [--ready-fd <n>] <config-path>`.
    Serve {
        path: PathBuf,
        /// Where readiness is reported (FR-PROC-07 step 3).
        ready: Option<ReadyFd>,
    },
    /// `durchlauferhitzer --check <config-path>`.
    Check(PathBuf),
}

/// Runs the binary with its arguments (without the program name): serves
/// or checks the config, and returns the exit code (FR-PROC-05a).
pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    ExitCode::from(start(args) as u8)
}

fn start(args: impl Iterator<Item = OsString>) -> ExitStatus {
    let Some(command) = parse_args(args) else {
        say_err(USAGE);
        return ExitStatus::Config;
    };
    match command {
        Command::Check(path) => check(&path),
        Command::Serve { path, ready } => serve(&path, ready),
    }
}

/// `<config-path>`, `--check <config-path>` or `--ready-fd <n>
/// <config-path>` (`n` decimal digits); anything else (an unknown flag:
/// any argument starting with `-` but those in front) is `None`.
fn parse_args(args: impl Iterator<Item = OsString>) -> Option<Command> {
    let args: Vec<OsString> = args.collect();
    let is_flag = |arg: &OsString| arg.as_encoded_bytes().starts_with(b"-");
    match args.as_slice() {
        [path] if !is_flag(path) => Some(Command::Serve {
            path: PathBuf::from(path),
            ready: None,
        }),
        [flag, path] if flag == "--check" && !is_flag(path) => {
            Some(Command::Check(PathBuf::from(path)))
        }
        [flag, n, path] if flag == "--ready-fd" && !is_flag(path) => Some(Command::Serve {
            path: PathBuf::from(path),
            ready: Some(ReadyFd::parse(n)?),
        }),
        _ => None,
    }
}

/// `--check`: loads the config and prints `ok` or the error; touches
/// neither the network nor the roots.
fn check(path: &Path) -> ExitStatus {
    match load_config(path) {
        Ok(_) => {
            let _ = writeln!(io::stdout().lock(), "ok");
            ExitStatus::Ok
        }
        Err(status) => status,
    }
}

/// Reads and loads the config (A11 step 2); a file that cannot be read is
/// a config error too (exit 1). The error is printed here.
fn load_config(path: &Path) -> Result<Config, ExitStatus> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        report(format_args!("{}: {err}", path.display()));
        ExitStatus::Config
    })?;
    load(&text).map_err(|err| {
        // `<path>:<line>:<column>: <message>` (FR-CFG-06).
        say_err(format_args!("{}:{err}", path.display()));
        ExitStatus::Config
    })
}

/// Serves `path`: takes the `--ready-fd` descriptor (before the config is
/// read, FR-PROC-07), loads the config, runs the startup sequence (A11),
/// then the event loop until it ends. A `--ready-fd` start binds without
/// the probe, joining the ports of the server it replaces.
fn serve(path: &Path, ready: Option<ReadyFd>) -> ExitStatus {
    let readiness = match ready.map(ReadyFd::claim).transpose() {
        Ok(readiness) => readiness,
        Err(Failure(message)) => {
            report(message);
            return ExitStatus::Startup;
        }
    };
    let config = match load_config(path) {
        Ok(config) => config,
        Err(status) => return status,
    };
    let probe = match readiness {
        Some(_) => Probe::Skip,
        None => Probe::First,
    };
    match start_up(config, probe) {
        Ok(ready) => run_loop(ready, readiness),
        Err(Failure(message)) => {
            report(message);
            ExitStatus::Startup
        }
    }
}

/// A startup failure (exit 2): the message printed after
/// `durchlauferhitzer: `.
#[derive(Debug)]
struct Failure(String);

impl From<containment::Error> for Failure {
    fn from(err: containment::Error) -> Self {
        Failure(err.to_string())
    }
}

/// Everything the loop needs, once startup has passed its checks.
#[derive(Debug)]
struct Ready {
    core: Core,
    listeners: Vec<OwnedFd>,
    /// In [`Core::static_roots`] order.
    roots: Vec<OwnedFd>,
    disk_threads: u32,
    /// `limits.shutdown_timeout`: how long the drain may take (FR-PROC-05).
    shutdown_timeout: Duration,
    /// `limits.restart_timeout`: how long the restart drain may take
    /// (FR-PROC-07).
    restart_timeout: Duration,
    /// Driver log entries of startup, written once the sink exists.
    notes: Vec<DriverLog>,
    /// The `containment` block (FR-CFG-25).
    containment: Containment,
    /// Landlock's outcome (A11 step 7a); the filter settles its gaps.
    landlock: Files,
}

/// A11 steps 3 to 7a: the `openat2` probe, the static roots, the
/// listeners (systemd's, A7, or bound here with `probe`, A6), the
/// descriptor budget (NFR-RES-05), the privilege drop (A8), then
/// Landlock (FR-PROC-06), while the process has one thread and before
/// anything else opens a path.
fn start_up(config: Config, probe: Probe) -> Result<Ready, Failure> {
    let containment = config.containment;
    let listen = config.listen.clone();
    let user = config.user.clone();
    let max_connections = config.limits.max_connections;
    let disk_threads = config.limits.disk_threads;
    let shutdown_timeout = config.limits.shutdown_timeout;
    let restart_timeout = config.limits.restart_timeout;
    let core = Core::new(config);
    let roots = open_roots(core.static_roots())?;
    let listeners = match take_sockets(&listen).map_err(Failure)? {
        Activation::Sockets(sockets) => sockets,
        Activation::None => bind(&listen, probe)?,
    };
    budget::ensure(budget::needed(budget::Descriptors {
        open: budget::open_now()?,
        max_connections,
        disk_threads,
        roots: roots.len(),
        backends: core.backends().len(),
    }))?;
    let notes = privileges::drop_privileges(user.as_deref())?
        .into_iter()
        .collect();
    let landlock = containment::restrict_files(&containment, &roots)?;
    Ok(Ready {
        core,
        listeners,
        roots,
        disk_threads,
        shutdown_timeout,
        restart_timeout,
        notes,
        containment,
        landlock,
    })
}

/// A11 steps 3 and 4: probes for `openat2` beneath the first root (or
/// `/`, opened `O_PATH`, when there is none), then opens every root
/// `O_PATH | O_DIRECTORY` in order. A root that cannot be opened is named.
fn open_roots(paths: &[PathBuf]) -> Result<Vec<OwnedFd>, Failure> {
    let open = |path: &Path| {
        open_dir_path(path).map_err(|err| Failure(format!("static root {}: {err}", path.display())))
    };
    let mut paths = paths.iter();
    let mut roots = Vec::with_capacity(paths.len());
    match paths.next() {
        Some(first) => {
            let root = open(first)?;
            probe_openat2(root.as_fd())?;
            roots.push(root);
        }
        None => {
            let slash = open_dir_path(Path::new("/"))
                .map_err(|err| Failure(format!("/ for the openat2 probe: {err}")))?;
            probe_openat2(slash.as_fd())?;
        }
    }
    for path in paths {
        roots.push(open(path)?);
    }
    Ok(roots)
}

/// A11 step 3, NFR-PORT-03: `openat2` must exist.
fn probe_openat2(dir: BorrowedFd<'_>) -> Result<(), Failure> {
    match openat2_supported(dir) {
        Ok(Openat2::Supported) => Ok(()),
        Ok(Openat2::Unsupported) => Err(Failure(
            "the kernel has no openat2 (Linux 5.6 or later is needed)".to_owned(),
        )),
        Err(err) => Err(Failure(format!("openat2 probe: {err}"))),
    }
}

/// A11 step 5 without systemd (A6): refuses overlapping addresses before
/// any socket (FR-CFG-10: `SO_REUSEPORT` would let a `--ready-fd` start
/// bind both), then binds every `listen` address in config order
/// (`SO_REUSEPORT`, after the probe unless it is skipped, FR-PROC-07);
/// the first failure names its address.
fn bind(listen: &[SocketAddr], probe: Probe) -> Result<Vec<OwnedFd>, Failure> {
    if let Some((_, later)) = overlap(listen) {
        let err = io::Error::from_raw_os_error(libc::EADDRINUSE);
        return Err(Failure(format!("cannot listen on {later}: {err}")));
    }
    listen
        .iter()
        .map(|&addr| {
            tcp_listen(addr, BACKLOG, probe)
                .map_err(|err| Failure(format!("cannot listen on {addr}: {err}")))
        })
        .collect()
}

/// The first pair of `listen` addresses (earlier, later in config order)
/// that cannot both be bound: the same family and port, one of them
/// unspecified (`0.0.0.0`, `::`). IPv6 listeners are IPv6-only, so the
/// families never overlap; config validation has refused duplicates.
fn overlap(listen: &[SocketAddr]) -> Option<(SocketAddr, SocketAddr)> {
    let clash = |a: &SocketAddr, b: &SocketAddr| {
        a.is_ipv4() == b.is_ipv4()
            && a.port() == b.port()
            && (a.ip().is_unspecified() || b.ip().is_unspecified())
    };
    listen.iter().enumerate().find_map(|(i, later)| {
        let earlier = listen
            .iter()
            .take(i)
            .find(|earlier| clash(earlier, later))?;
        Some((*earlier, *later))
    })
}

/// A11 steps 8 to 11: the signal mask and its signalfd (before the disk
/// workers start, so they inherit the mask; A9), the stderr log sink, the
/// driver reading the signals, the seccomp filter (FR-PROC-06, step 10a:
/// once the disk workers run, right before the loop) and the containment
/// lines, readiness on the `--ready-fd` descriptor (FR-PROC-07; a failed
/// write is logged, `kind=ready_failed`), then the loop. A failure
/// before that closes the descriptor unwritten. Every message is printed
/// while no sink lives:
/// before it is created, or after the driver holding it is gone. Every
/// ending of the loop exits 0, a drain, an abort or a stop (A9); a
/// failure of the loop itself, or of a containment layer, exits 2.
fn run_loop(ready: Ready, readiness: Option<Readiness>) -> ExitStatus {
    let Ready {
        core,
        listeners,
        roots,
        disk_threads,
        shutdown_timeout,
        restart_timeout,
        notes,
        containment,
        landlock,
    } = ready;
    // SIGTERM, SIGINT, SIGPIPE and SIGHUP: blocked in every thread, read
    // by the loop (SIGPIPE ignored there).
    let signals = match SignalFd::new(&Signal::HANDLED) {
        Ok(signals) => signals,
        Err(err) => {
            report(format_args!("signalfd: {err}"));
            return ExitStatus::Startup;
        }
    };
    let mut log = match LogSink::stderr() {
        Ok(log) => log,
        Err(err) => {
            report(format_args!("log sink on stderr: {err}"));
            return ExitStatus::Startup;
        }
    };
    // Written before `Driver::new` (review M6, accepted): the driver owns
    // the sink from then on. If `new` fails, the `privileges` line stands
    // before the error line; both are true.
    for note in notes {
        log.write(LogEntry::Driver(note));
    }
    // On failure `new` has dropped the sink already.
    let mut driver = match Driver::new(core, listeners, roots, disk_threads, log) {
        Ok(driver) => driver,
        Err(err) => {
            report(format_args!("driver: {err}"));
            return ExitStatus::Startup;
        }
    };
    if let Err(err) = driver.watch_signals(signals, shutdown_timeout, restart_timeout) {
        drop(driver);
        report(format_args!("event loop: {err}"));
        return ExitStatus::Startup;
    }
    match containment::restrict_calls(&containment, landlock) {
        Ok(outcomes) => {
            for outcome in outcomes {
                driver.write_log(DriverLog::Containment(outcome));
            }
        }
        Err(err) => {
            // The sink goes first (FR-LOG-06).
            drop(driver);
            report(err);
            return ExitStatus::Startup;
        }
    }
    // Readiness is the last step of startup: nothing that can fail may
    // follow it, or the old process would get SIGHUP for a dead one.
    if let Some(readiness) = readiness
        && let Err(err) = readiness.report()
    {
        let errno = err.raw_os_error().unwrap_or(0);
        driver.write_log(DriverLog::ReadyFailed { errno });
    }
    let result = driver.run();
    // The sink goes with the driver (the signalfd too); only then may std
    // print again.
    drop(driver);
    match result {
        Ok(Exit::Stopped | Exit::Drained | Exit::Aborted) => ExitStatus::Ok,
        Err(err) => {
            report(format_args!("event loop: {err}"));
            ExitStatus::Startup
        }
    }
}

/// Prints `durchlauferhitzer: <message>` to stderr.
fn report(message: impl Display) {
    say_err(format_args!("durchlauferhitzer: {message}"));
}

/// Prints one line to stderr; a failing stderr is ignored (there is
/// nowhere else to say it).
fn say_err(line: impl Display) {
    let _ = writeln!(io::stderr().lock(), "{line}");
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use std::net::SocketAddr;

    use super::{Command, ReadyFd, overlap, parse_args};

    fn parse(args: &[&str]) -> Option<Command> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn arguments() {
        let serve = |ready| {
            Some(Command::Serve {
                path: PathBuf::from("a.conf"),
                ready,
            })
        };
        assert_eq!(parse(&["a.conf"]), serve(None));
        assert_eq!(
            parse(&["--ready-fd", "3", "a.conf"]),
            serve(ReadyFd::parse(std::ffi::OsStr::new("3")))
        );
        assert_eq!(
            parse(&["--check", "a.conf"]),
            Some(Command::Check(PathBuf::from("a.conf")))
        );
        for bad in [
            &[][..],
            &["--check"],
            &["-a.conf"],
            &["--check", "-a.conf"],
            &["--check", "--check"],
            &["a.conf", "--check"],
            &["a", "b"],
            &["--check", "a", "b"],
            &["--ready-fd"],
            &["--ready-fd", "a.conf"],
            &["--ready-fd", "x", "a.conf"],
            &["--ready-fd", "3", "-a.conf"],
            &["--ready-fd", "3", "--check", "a.conf"],
            &["--check", "--ready-fd", "3", "a.conf"],
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    /// FR-CFG-10: an unspecified address and another of its family on the
    /// same port overlap; the pair is (earlier, later) in config order, the
    /// first later one that overlaps any earlier one.
    #[test]
    fn overlapping_listen_addresses() {
        let addrs = |list: &[&str]| -> Vec<SocketAddr> {
            list.iter().map(|addr| addr.parse().unwrap()).collect()
        };
        let pair = |a: &str, b: &str| Some((a.parse().unwrap(), b.parse().unwrap()));
        type Pair = Option<(SocketAddr, SocketAddr)>;
        let cases: [(&[&str], Pair); 10] = [
            (
                &["0.0.0.0:80", "127.0.0.1:80"],
                pair("0.0.0.0:80", "127.0.0.1:80"),
            ),
            (
                &["127.0.0.1:80", "0.0.0.0:80"],
                pair("127.0.0.1:80", "0.0.0.0:80"),
            ),
            (&["[::]:80", "[::1]:80"], pair("[::]:80", "[::1]:80")),
            (&["[::1]:80", "[::]:80"], pair("[::1]:80", "[::]:80")),
            (
                &["127.0.0.1:81", "0.0.0.0:80", "127.0.0.2:80", "127.0.0.1:80"],
                pair("0.0.0.0:80", "127.0.0.2:80"),
            ),
            // IPv6 listeners are IPv6-only: no overlap across families.
            (&["0.0.0.0:80", "[::]:80"], None),
            (&["127.0.0.1:80", "127.0.0.2:80"], None),
            (&["[::1]:80", "[fe80::1]:80"], None),
            (&["0.0.0.0:80", "127.0.0.1:81"], None),
            (&["127.0.0.1:80"], None),
        ];
        for (list, expected) in cases {
            assert_eq!(overlap(&addrs(list)), expected, "{list:?}");
        }
    }
}
