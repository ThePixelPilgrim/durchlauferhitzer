//! The one skip rule for the IPv6 tests, shared by the unit tests in
//! `src/sys/socket.rs` and the integration tests in `tests/sys.rs` (both
//! include this file with `#[path]`).
//!
//! A host without IPv6 (`EAFNOSUPPORT`, or no `::1`: `EADDRNOTAVAIL`) skips
//! the test with a printed reason, unless `DURCHLAUFERHITZER_REQUIRE_IPV6=1`
//! is set (CI sets it): then the missing IPv6 is a failure, so a CI run can
//! never pass by skipping. Any other error is a failure.

#![allow(clippy::panic, reason = "test helper outside #[test] functions")]

use std::io;

/// The environment variable that turns a skip into a failure.
const REQUIRE: &str = "DURCHLAUFERHITZER_REQUIRE_IPV6";

/// `Some` with the value, or `None` when the test should skip.
pub fn or_skip<T>(result: io::Result<T>) -> Option<T> {
    let err = match result {
        Ok(value) => return Some(value),
        Err(err) => err,
    };
    let no_ipv6 = err.raw_os_error() == Some(libc::EAFNOSUPPORT)
        || err.raw_os_error() == Some(libc::EADDRNOTAVAIL);
    if !no_ipv6 {
        panic!("IPv6 setup failed: {err}");
    }
    if std::env::var_os(REQUIRE).is_some_and(|v| v == "1") {
        panic!("no IPv6 here, but {REQUIRE}=1 requires it: {err}");
    }
    eprintln!("skipped: no IPv6 here ({err})");
    None
}
