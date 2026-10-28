//! The `durchlauferhitzer` binary: the command line and startup sequence
//! are `durchlauferhitzer::process::run` (FR-PROC-01, FR-PROC-04).

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    durchlauferhitzer::process::run(std::env::args_os().skip(1))
}
