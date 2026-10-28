//! The process: the command line (FR-PROC-01), the startup sequence
//! (FR-PROC-04, A11) and the exit codes (FR-PROC-05a).
//!
//! Everything this module prints (the usage, `--check`'s answer, every
//! startup error) is printed before the stderr log sink exists, and
//! nothing is printed through std while it lives (FR-LOG-06): the sink
//! makes fd 2's file description non-blocking, which stdout may share.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use std::net::SocketAddr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use durchlauferhitzer_core::Core;
use durchlauferhitzer_core::config::{Config, load};

use crate::event_loop::{Driver, Exit};
use crate::log::LogSink;
use crate::sys::file::{Openat2, open_dir_path, openat2_supported};
use crate::sys::signal::{Signal, SignalFd};
use crate::sys::socket::tcp_listen;
use crate::systemd::{Activation, take_sockets};

/// The usage text, printed to stderr for bad arguments (exit 1).
const USAGE: &str =
    "usage: durchlauferhitzer <config-path>\n       durchlauferhitzer --check <config-path>";

/// The listen backlog: `SOMAXCONN` (the kernel caps it at
/// `net.core.somaxconn`).
const BACKLOG: u32 = libc::SOMAXCONN as u32;

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
    /// `durchlauferhitzer <config-path>`.
    Serve(PathBuf),
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
        Command::Serve(path) => serve(&path),
    }
}

/// `<config-path>` or `--check <config-path>`; anything else (an unknown
/// flag: any argument starting with `-` but `--check` in front) is `None`.
fn parse_args(args: impl Iterator<Item = OsString>) -> Option<Command> {
    let args: Vec<OsString> = args.collect();
    let is_flag = |arg: &OsString| arg.as_encoded_bytes().starts_with(b"-");
    match args.as_slice() {
        [path] if !is_flag(path) => Some(Command::Serve(PathBuf::from(path))),
        [flag, path] if flag == "--check" && !is_flag(path) => {
            Some(Command::Check(PathBuf::from(path)))
        }
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

/// Serves `path`: loads the config, runs the startup sequence (A11), then
/// the event loop until it ends.
fn serve(path: &Path) -> ExitStatus {
    let config = match load_config(path) {
        Ok(config) => config,
        Err(status) => return status,
    };
    match start_up(config) {
        Ok(ready) => run_loop(ready),
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

/// Everything the loop needs, once startup has passed its checks.
#[derive(Debug)]
struct Ready {
    core: Core,
    listeners: Vec<OwnedFd>,
    /// In [`Core::static_roots`] order.
    roots: Vec<OwnedFd>,
    disk_threads: u32,
}

/// A11 steps 3 to 7: the `openat2` probe, the static roots, the listeners
/// (systemd's, A7, or bound here, A6).
fn start_up(config: Config) -> Result<Ready, Failure> {
    let listen = config.listen.clone();
    let disk_threads = config.limits.disk_threads;
    let core = Core::new(config);
    let roots = open_roots(core.static_roots())?;
    let listeners = match take_sockets(&listen).map_err(Failure)? {
        Activation::Sockets(sockets) => sockets,
        Activation::None => bind(&listen)?,
    };
    Ok(Ready {
        core,
        listeners,
        roots,
        disk_threads,
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

/// A11 step 5 without systemd (A6): binds every `listen` address in config
/// order; the first failure names its address.
fn bind(listen: &[SocketAddr]) -> Result<Vec<OwnedFd>, Failure> {
    listen
        .iter()
        .map(|&addr| {
            tcp_listen(addr, BACKLOG)
                .map_err(|err| Failure(format!("cannot listen on {addr}: {err}")))
        })
        .collect()
}

/// A11 steps 8 and 9: the signal mask and its signalfd (before the disk
/// workers start, so they inherit the mask; A9), the stderr log sink, the
/// driver, then the loop. Every message is printed while no sink lives:
/// before it is created, or after the driver holding it is gone.
fn run_loop(ready: Ready) -> ExitStatus {
    let Ready {
        core,
        listeners,
        roots,
        disk_threads,
    } = ready;
    // Signal handling is Task 9's; until then the signals stay blocked
    // and pending, and SIGPIPE can never kill the process.
    let signals = match SignalFd::new(&[Signal::Term, Signal::Int, Signal::Pipe]) {
        Ok(signals) => signals,
        Err(err) => {
            report(format_args!("signalfd: {err}"));
            return ExitStatus::Startup;
        }
    };
    let log = match LogSink::stderr() {
        Ok(log) => log,
        Err(err) => {
            report(format_args!("log sink on stderr: {err}"));
            return ExitStatus::Startup;
        }
    };
    // On failure `new` has dropped the sink already.
    let mut driver = match Driver::new(core, listeners, roots, disk_threads, log) {
        Ok(driver) => driver,
        Err(err) => {
            report(format_args!("driver: {err}"));
            return ExitStatus::Startup;
        }
    };
    let result = driver.run();
    // The sink goes with the driver; only then may std print again.
    drop(driver);
    drop(signals);
    match result {
        Ok(Exit::Stopped) => ExitStatus::Ok,
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

    use super::{Command, parse_args};

    fn parse(args: &[&str]) -> Option<Command> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse(&["a.conf"]),
            Some(Command::Serve(PathBuf::from("a.conf")))
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
        ] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }
}
