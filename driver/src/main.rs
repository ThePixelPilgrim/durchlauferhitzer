//! The `durchlauferhitzer` binary. Stage 4 Task 4: a stub that prints the
//! usage; the CLI and startup sequence follow in Task 8 (FR-PROC-01).

#![deny(unsafe_code)]

use std::process::ExitCode;

const USAGE: &str =
    "usage: durchlauferhitzer <config-path>\n       durchlauferhitzer --check <config-path>";

fn main() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::from(1)
}
