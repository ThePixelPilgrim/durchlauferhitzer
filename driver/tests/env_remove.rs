//! `sys::env::remove_vars` removes the variables (A7: the systemd
//! variables are cleared after use). It works only while the process has
//! one thread, so this test has no libtest harness: `main` runs itself
//! again with the variables set, and the child, still single-threaded,
//! removes them and checks they are gone.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "a test binary; clippy.toml allows these in tests"
)]

use std::process::{Command, ExitCode};

use durchlauferhitzer::sys::env::remove_vars;

const NAMES: [&str; 3] = ["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"];
const CHILD: &str = "DURCHLAUFERHITZER_ENV_CHILD";

fn main() -> ExitCode {
    if std::env::var_os(CHILD).is_some() {
        return child();
    }
    let status = Command::new(std::env::current_exe().unwrap())
        .env(CHILD, "1")
        .env("LISTEN_PID", "1")
        .env("LISTEN_FDS", "1")
        .env("LISTEN_FDNAMES", "web")
        .status()
        .unwrap();
    if status.success() {
        println!("test remove_vars_removes_the_variables ... ok");
        ExitCode::SUCCESS
    } else {
        println!("test remove_vars_removes_the_variables ... FAILED ({status})");
        ExitCode::FAILURE
    }
}

fn child() -> ExitCode {
    if NAMES.iter().any(|name| std::env::var_os(name).is_none()) {
        eprintln!("the variables were not passed");
        return ExitCode::FAILURE;
    }
    if let Err(err) = remove_vars(&NAMES) {
        eprintln!("remove_vars: {err}");
        return ExitCode::FAILURE;
    }
    match NAMES.iter().find(|name| std::env::var_os(name).is_some()) {
        Some(name) => {
            eprintln!("{name} is still set");
            ExitCode::FAILURE
        }
        None => ExitCode::SUCCESS,
    }
}
