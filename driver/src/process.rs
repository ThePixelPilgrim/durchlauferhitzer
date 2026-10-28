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

use durchlauferhitzer_core::config::{Config, load};

/// The usage text, printed to stderr for bad arguments (exit 1).
const USAGE: &str =
    "usage: durchlauferhitzer <config-path>\n       durchlauferhitzer --check <config-path>";

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

fn serve(path: &Path) -> ExitStatus {
    match load_config(path) {
        Ok(_) => {
            report("serving is not implemented yet");
            ExitStatus::Startup
        }
        Err(status) => status,
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
